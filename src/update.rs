//! 核心更新逻辑。
//!
//! 通过 GitHub Releases API 检查 sing-box / xray 的最新版本,
//! 与本地版本比较后决定是否下载更新。支持 CDN 代理、SHA256 校验、
//! 自动重试, 下载完成后从 zip 中提取 exe 并替换。

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::error::AppError;
use crate::state::{Core, CoreArtifacts};

/// GitHub API 要求的 User-Agent 头, 缺少会返回 403。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Edg/149.0.0.0";

/// 计算哈希时的读取缓冲区大小。
///
/// 核心 zip 和规则集 dat 都在 10~30 MB 量级, 64 KB 相比 8 KB 能把 read 系统
/// 调用次数降到八分之一。
const HASH_BUFFER_SIZE: usize = 64 * 1024;

// ureq 3 的所有超时默认都是 None (含 DNS 解析、建连和读取) , 不显式设置的话
// 服务器接受连接后不发数据就会永久挂起, 下载线程永远不返回。

/// GitHub API、校验和等小响应请求的端到端超时。
const SMALL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 文件下载的建连超时, 含 DNS 解析与 TLS 握手。
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 文件下载接收响应头的超时。
const DOWNLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// 文件下载接收响应体的总时长上限。
///
/// ureq 只提供总时长上限, 没有空闲超时。核心 zip 与规则集 dat 通常在
/// 10~30 MB, 10 分钟对应约 20~50 KB/s 的速度下限, 比这更慢时重试也无意义。
const DOWNLOAD_BODY_TIMEOUT: Duration = Duration::from_secs(600);

/// 发起带超时的 GET 请求并把响应体读成字符串, 用于 GitHub API 和校验和文件。
fn get_text(url: &str) -> Result<String, AppError> {
    let resp = ureq::get(url)
        .config()
        .timeout_global(Some(SMALL_REQUEST_TIMEOUT))
        .build()
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| AppError::Msg(format!("请求失败: {e}")))?;

    resp.into_body()
        .read_to_string()
        .map_err(|e| AppError::Msg(format!("读取响应失败: {e}")))
}

// 编译时根据目标架构确定下载文件名。
// amd64 编译产物只下载 amd64 核心, arm64 编译产物只下载 arm64 核心。
const SINGBOX_ARCH_SUFFIX: &str = if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "amd64"
};

const XRAY_ZIP_NAME: &str = if cfg!(target_arch = "aarch64") {
    "Xray-windows-arm64-v8a.zip"
} else {
    "Xray-windows-64.zip"
};

/// 依次更新 sing-box 和 xray。
pub fn update_cores(exe_dir: &Path, gh_proxy_url: &str, max_retries: u32, delay_secs: u64) -> Result<(), AppError> {
    update_sing_box(exe_dir, gh_proxy_url, max_retries, delay_secs)?;
    update_xray(exe_dir, gh_proxy_url, max_retries, delay_secs)
}

/// 检查并更新 sing-box。
pub fn update_sing_box(exe_dir: &Path, gh_proxy_url: &str, max_retries: u32, delay_secs: u64) -> Result<(), AppError> {
    let exe_path = Core::SingBox.exe_path(exe_dir);

    let local = get_local_version(&exe_path, "version");
    let (remote_ver, assets) = fetch_sing_box_release()?;
    debug!("[sing-box] 版本比较: local={local}, remote={remote_ver}");

    if !is_newer(&local, &remote_ver) {
        info!("[sing-box] 已是最新版本, 跳过更新");
        crate::toast::show_toast("sing-box", "当前已是最新版本");
        return Ok(());
    }

    let zip_name = format!("sing-box-{}-windows-{}.zip", remote_ver, SINGBOX_ARCH_SUFFIX);
    let zip_path = exe_dir.join(&zip_name);

    let (download_url, expected_hash) = find_asset(&assets, &zip_name)?;
    debug!("[sing-box] 下载链接: {download_url}, SHA256: {expected_hash}");

    crate::toast::show_toast("sing-box", &format!("检测到新版本 v{remote_ver}"));

    let tag = "update-sing-box";
    let title = format!("sing-box v{remote_ver}");
    crate::toast::show_progress_toast(&title, tag);

    if let Err(e) = download_core_with_retry(
        &download_url,
        &zip_path,
        &expected_hash,
        gh_proxy_url,
        max_retries,
        delay_secs,
    ) {
        error!("[sing-box] 下载失败: {e}");
        crate::toast::show_toast_tagged("sing-box", "下载失败, 请稍后重试", tag);
        // 刻意返回 Ok: 一是 update_cores 用 ? 串联两个核心, 返回 Err 会跳过
        // xray 的更新; 二是上面的 tagged toast 已经就地通知过用户, 再返回
        // Err 会让调用方多弹一个"更新失败"。
        return Ok(());
    }

    let core = Core::SingBox;
    extract_artifacts(&zip_path, &core.core_dir(exe_dir), &core.artifacts())?;

    let _ = fs::remove_file(&zip_path);
    info!("[sing-box] 更新完成 -> v{remote_ver}");
    crate::toast::show_toast_tagged("sing-box", "更新完成", tag);
    Ok(())
}

/// 检查并更新 xray。
pub fn update_xray(exe_dir: &Path, gh_proxy_url: &str, max_retries: u32, delay_secs: u64) -> Result<(), AppError> {
    let exe_path = Core::Xray.exe_path(exe_dir);

    let local = get_local_version(&exe_path, "version");
    let (remote_ver, assets) = fetch_xray_release(XRAY_ZIP_NAME)?;
    debug!("[xray] 版本比较: local={local}, remote={remote_ver}");

    if !is_newer(&local, &remote_ver) {
        info!("[xray] 已是最新版本, 跳过更新");
        crate::toast::show_toast("xray", "当前已是最新版本");
        return Ok(());
    }

    let zip_name = XRAY_ZIP_NAME;
    let zip_path = exe_dir.join(zip_name);

    let (download_url, expected_hash) = find_asset(&assets, zip_name)?;
    debug!("[xray] 下载链接: {download_url}, SHA256: {expected_hash}");

    crate::toast::show_toast("xray", &format!("检测到新版本 v{remote_ver}"));

    let tag = "update-xray";
    let title = format!("xray v{remote_ver}");
    crate::toast::show_progress_toast(&title, tag);

    if let Err(e) = download_core_with_retry(
        &download_url,
        &zip_path,
        &expected_hash,
        gh_proxy_url,
        max_retries,
        delay_secs,
    ) {
        error!("[xray] 下载失败: {e}");
        crate::toast::show_toast_tagged("xray", "下载失败, 请稍后重试", tag);
        // 与 update_sing_box 同理, 刻意返回 Ok, 见那里的说明
        return Ok(());
    }

    let core = Core::Xray;
    extract_artifacts(&zip_path, &core.core_dir(exe_dir), &core.artifacts())?;

    let _ = fs::remove_file(&zip_path);
    info!("[xray] 更新完成 -> v{remote_ver}");
    crate::toast::show_toast_tagged("xray", "更新完成", tag);
    Ok(())
}

/// 运行可执行文件的版本命令并从 stdout 提取版本号, 失败返回 "0.0.0"。
///
/// 必须用 `hidden_command`: sing-box 和 xray 都是控制台程序, release 构建的
/// 本程序是 GUI 子系统, 用裸 Command 启动会分配新控制台, 每次检查版本闪一次
/// 黑窗。debug 构建有控制台可继承, 因此这个问题只在 release 下可见。
pub(crate) fn get_local_version(exe_path: &Path, version_arg: &str) -> String {
    let output = match crate::process::hidden_command(exe_path).arg(version_arg).output() {
        Ok(out) => out,
        Err(e) => {
            warn!("获取版本失败 ({}): {e}", exe_path.display());
            return "0.0.0".to_string();
        }
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let version = extract_version(&text).unwrap_or_else(|| "0.0.0".to_string());
    debug!("本地版本: {} -> {version}", exe_path.display());
    version
}

/// 从命令输出中提取版本号 (如 "sing-box version 1.13.13" → "1.13.13") 。
///
/// 逐字节扫描, 累积数字和点号, 遇到连字符停止 (跳过 "-beta" 等后缀) ,
/// 遇到其他非数字字符终止。要求结果至少包含一个点号。
fn extract_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = None;
    let mut end = None;

    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() || b == b'.' {
            if start.is_none() {
                start = Some(i);
            }
            end = Some(i + 1);
        } else if start.is_some() && b != b'-' {
            break;
        }
    }

    match (start, end) {
        (Some(s), Some(e)) => {
            let version = &text[s..e];
            if version.contains('.') {
                Some(version.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 比较两个版本号, remote > local 时返回 true。
/// 缺失的段按 0 处理, 因此 "1.0" 等价于 "1.0.0"。
fn is_newer(local: &str, remote: &str) -> bool {
    let local: Vec<u32> = local.split('.').filter_map(|s| s.parse().ok()).collect();
    let remote: Vec<u32> = remote.split('.').filter_map(|s| s.parse().ok()).collect();

    for i in 0..local.len().max(remote.len()) {
        let l = local.get(i).copied().unwrap_or(0);
        let r = remote.get(i).copied().unwrap_or(0);
        if r > l {
            return true;
        }
        if r < l {
            return false;
        }
    }
    false
}

/// 调用 GitHub Releases API 获取 sing-box 最新正式版的版本号和 assets 列表。
fn fetch_sing_box_release() -> Result<(String, Vec<Value>), AppError> {
    let api_url = "https://api.github.com/repos/SagerNet/sing-box/releases/latest";
    debug!("请求 GitHub API: {api_url}");
    let body = get_text(api_url).map_err(|e| AppError::Msg(format!("请求 GitHub API 失败: {e}")))?;

    let json: Value =
        serde_json::from_str(&body).map_err(|e| AppError::Msg(format!("解析 GitHub API 响应失败: {e}")))?;

    let tag = json["tag_name"]
        .as_str()
        .ok_or(AppError::Msg("GitHub API 响应缺少 tag_name".into()))?
        .to_string();

    let version = tag.trim_start_matches('v').to_string();
    let assets: Vec<Value> = json["assets"].as_array().cloned().unwrap_or_default();

    debug!("GitHub API 响应: tag={tag}, assets 数量={}", assets.len());
    Ok((version, assets))
}

/// 调用 GitHub Releases API 获取 xray 最新版本（含 pre-release）的版本号和 assets 列表。
///
/// 遍历 `/releases` 返回的列表，跳过 draft，返回第一个包含 `zip_name` 的 release。
fn fetch_xray_release(zip_name: &str) -> Result<(String, Vec<Value>), AppError> {
    let api_url = "https://api.github.com/repos/XTLS/Xray-core/releases";
    debug!("请求 GitHub API: {api_url}");
    let body = get_text(api_url).map_err(|e| AppError::Msg(format!("请求 GitHub API 失败: {e}")))?;

    let releases: Vec<Value> =
        serde_json::from_str(&body).map_err(|e| AppError::Msg(format!("解析 GitHub API 响应失败: {e}")))?;

    for release in &releases {
        if release["draft"].as_bool() == Some(true) {
            continue;
        }
        let assets = release["assets"].as_array().cloned().unwrap_or_default();
        if assets.iter().any(|a| a["name"].as_str() == Some(zip_name)) {
            let tag = release["tag_name"]
                .as_str()
                .ok_or(AppError::Msg("GitHub API 响应缺少 tag_name".into()))?
                .to_string();
            let version = tag.trim_start_matches('v').to_string();
            debug!("GitHub API 响应: tag={tag}, assets 数量={}", assets.len());
            return Ok((version, assets));
        }
    }

    Err(AppError::Msg(format!("未找到包含 {zip_name} 的 release")))
}

/// 从 release assets 中查找指定文件名的下载 URL 和 digest 哈希值。
///
/// 未找到文件或 digest 缺失时返回错误。
fn find_asset(assets: &[Value], file_name: &str) -> Result<(String, String), AppError> {
    let asset = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(file_name))
        .ok_or_else(|| AppError::Msg(format!("未找到发布文件: {file_name}")))?;

    let url = asset["browser_download_url"]
        .as_str()
        .ok_or_else(|| AppError::Msg(format!("发布文件缺少下载链接: {file_name}")))?
        .to_string();

    let digest = asset["digest"]
        .as_str()
        .ok_or_else(|| AppError::Msg(format!("发布文件缺少 digest: {file_name}")))?;
    let hash = digest
        .split(':')
        .next_back()
        .ok_or_else(|| AppError::Msg(format!("digest 格式无效: {digest}")))?
        .to_string();

    Ok((url, hash))
}

/// 带重试的下载。启用代理时将代理 URL 前缀拼接到下载链接。
/// 下载后校验 SHA256, 不匹配则删除文件并重试。
fn download_core_with_retry(
    download_url: &str,
    dest: &Path,
    expected_hash: &str,
    gh_proxy_url: &str,
    max_retries: u32,
    delay_secs: u64,
) -> Result<(), AppError> {
    let url = if gh_proxy_url.is_empty() {
        download_url.to_string()
    } else {
        format!("{gh_proxy_url}{download_url}")
    };
    debug!(
        "下载准备: url={url}, 代理={}, hash={expected_hash}",
        if gh_proxy_url.is_empty() { "禁用" } else { "启用" },
    );

    for attempt in 1..=max_retries {
        if attempt > 1 {
            debug!("第 {attempt}/{max_retries} 次重试, 等待 {delay_secs}s...");
            std::thread::sleep(std::time::Duration::from_secs(delay_secs));
        } else {
            debug!("第 1/{max_retries} 次尝试下载...");
        }

        if let Err(e) = download_file(&url, dest) {
            warn!("下载失败 (第 {attempt}/{max_retries} 次): {e}");
            let _ = fs::remove_file(dest);
            continue;
        }

        let actual = match sha256_file(dest) {
            Ok(h) => h,
            Err(e) => {
                warn!("SHA256 计算失败 (第 {attempt}/{max_retries} 次): {e}");
                let _ = fs::remove_file(dest);
                continue;
            }
        };
        if actual.eq_ignore_ascii_case(expected_hash) {
            info!("SHA256 校验通过: {actual}");
            return Ok(());
        }
        warn!("SHA256 校验失败: expected={expected_hash}, actual={actual}");
        let _ = fs::remove_file(dest);
    }

    Err(AppError::Msg("下载文件校验失败, 已达到最大重试次数".into()))
}

/// 下载单个文件到指定路径, 已存在时先删除。
fn download_file(url: &str, dest: &Path) -> Result<(), AppError> {
    if dest.exists() {
        let _ = fs::remove_file(dest);
    }

    let resp = ureq::get(url)
        .config()
        .timeout_connect(Some(DOWNLOAD_CONNECT_TIMEOUT))
        .timeout_recv_response(Some(DOWNLOAD_RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(DOWNLOAD_BODY_TIMEOUT))
        .build()
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| AppError::Msg(format!("下载失败: {e}")))?;

    let mut reader = resp.into_body().into_reader();
    let mut file = fs::File::create(dest).map_err(|e| AppError::Msg(format!("创建文件失败: {e}")))?;

    let bytes = io::copy(&mut reader, &mut file).map_err(|e| AppError::Msg(format!("写入文件失败: {e}")))?;
    info!("下载完成: {} ({:.1} MB)", dest.display(), bytes as f64 / 1_048_576.0);

    Ok(())
}

/// 计算文件的 SHA256 哈希值, 返回小写十六进制字符串。
fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = fs::File::open(path).map_err(|e| AppError::Msg(format!("打开文件计算 SHA256 失败: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| AppError::Msg(format!("读取文件计算 SHA256 失败: {e}")))?;
        if n == 0 {
            break;
        }
        Digest::update(&mut hasher, &buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 把字节序列编码成小写十六进制字符串。
///
/// 逐字节 `format!` 会为每个字节分配一个 String, 这里一次分配到位。
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// 替换核心文件时给旧文件加的后缀。
///
/// 替换成功后刻意不删: 留一份供回退, 直到下一次更新确认可以替换时才清理。
const OLD_SUFFIX: &str = "old";

/// 解压过程中给新文件加的后缀。
const NEW_SUFFIX: &str = "new";

/// 改名的总尝试次数 (含第一次) 。
///
/// 刚落地的文件立刻要改名, 而杀软实时扫描、索引器、同步盘, 以及还没完全退出
/// 的核心进程 (`TerminateProcess` 是异步的) 都可能短暂持有句柄, 让改名以共享
/// 冲突失败。这类占用是毫秒级的, 重试几次就能过去。
const RENAME_ATTEMPTS: u32 = 10;

/// 改名重试的间隔。
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(300);

/// 带重试的改名。
///
/// 源文件不存在时立即返回: 那不是占用问题, 等下去也不会好转。
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    for attempt in 1..RENAME_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
            Err(e) => debug!(
                "改名失败 (第 {attempt}/{RENAME_ATTEMPTS} 次) {} -> {}: {e}",
                from.display(),
                to.display()
            ),
        }
        std::thread::sleep(RENAME_RETRY_DELAY);
    }
    fs::rename(from, to)
}

/// 从 zip 中提取核心运行所需的文件, 核心目录里的其他内容保持不动。
///
/// 不再整体替换核心目录, 原因见 `CoreArtifacts` 的说明 —— 那样会盖掉用户
/// 配置的规则集 dat, 也会丢掉 sing-box 运行时生成的 cache.db。
///
/// 流程:
/// 1. 清理可能残留的 `.new`
/// 2. 按文件名匹配清单, 命中的先解压成 `{name}.new`
/// 3. `required` 有缺失就清掉 `.new` 并报错, 核心目录保持原状
/// 4. 确认可以替换后, 才清理上一次更新留下的 `.old`
/// 5. 逐个替换: 旧文件改名成 `{name}.old`, 再把 `{name}.new` 改名到位,
///    中途失败则把本轮已改名的 `.old` 恢复回去
///
/// 第 5 步先挪开再放新的, 而不是直接覆盖: Windows 允许改名正在运行的 exe,
/// 却不允许覆盖它, 而 `TerminateProcess` 是异步的, `stop_all` 返回时进程
/// 可能还没完全退出。
///
/// 按文件名匹配而不是按 zip 内路径: sing-box 的包有一层带版本号的顶层目录,
/// 原先靠拼 `sing-box-{ver}-windows-{arch}/` 前缀剥离, 官方一旦改了命名规则,
/// 所有条目都会被静默跳过 —— 更新"成功"却一个文件也没换。
fn extract_artifacts(zip_path: &Path, core_dir: &Path, artifacts: &CoreArtifacts) -> Result<(), AppError> {
    fs::create_dir_all(core_dir).map_err(|e| AppError::Msg(format!("创建核心目录失败: {e}")))?;
    remove_suffixed(core_dir, artifacts, NEW_SUFFIX);

    match stage_and_replace(zip_path, core_dir, artifacts) {
        Ok(()) => Ok(()),
        Err(e) => {
            remove_suffixed(core_dir, artifacts, NEW_SUFFIX);
            Err(e)
        }
    }
}

/// 解压到 `.new` 并逐个替换到位, 失败时由调用方清理 `.new`。
///
/// 替换中途失败会先回滚已改名的旧文件, 再把错误返回给调用方。
fn stage_and_replace(zip_path: &Path, core_dir: &Path, artifacts: &CoreArtifacts) -> Result<(), AppError> {
    let file = fs::File::open(zip_path).map_err(|e| AppError::Msg(format!("打开 zip 文件失败: {e}")))?;
    let reader = BufReader::new(file);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| AppError::Msg(format!("解析 zip 文件失败: {e}")))?;

    let mut staged: Vec<String> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::Msg(format!("读取 zip 条目失败: {e}")))?;

        if entry.is_dir() {
            continue;
        }

        // enclosed_name 会拒绝绝对路径、含 NULL 字节, 以及用 .. 逃出目标目录的
        // 条目。不能改用 name() 自行拼接后 starts_with 判断: Path::starts_with
        // 是按组件的词法比较、不解析 .., core_dir/../evil 会被判定为子路径。
        let Some(entry_path) = entry.enclosed_name() else {
            warn!("zip 条目路径不安全, 跳过: {}", entry.name());
            continue;
        };

        let Some(name) = entry_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !artifacts.wants(name) {
            continue;
        }
        if staged.iter().any(|s| s == name) {
            warn!("zip 中存在重名文件 {name}, 忽略后一个");
            continue;
        }

        let tmp_path = core_dir.join(format!("{name}.{NEW_SUFFIX}"));
        let mut out = fs::File::create(&tmp_path).map_err(|e| AppError::Msg(format!("创建文件失败: {e}")))?;
        io::copy(&mut entry, &mut out).map_err(|e| AppError::Msg(format!("解压文件失败: {e}")))?;
        debug!("已解压 {name} -> {}", tmp_path.display());
        staged.push(name.to_string());
    }

    for name in artifacts.required {
        if !staged.iter().any(|s| s == name) {
            return Err(AppError::Msg(format!("zip 中缺少必需文件: {name}")));
        }
    }
    for name in artifacts.optional {
        if !staged.iter().any(|s| s == name) {
            warn!("zip 中未找到 {name}, 本次不更新该文件");
        }
    }

    // 清理上一轮的 `.old` 刻意放在这里, 而不是解压之前: 上面任何一步失败都会
    // 让本次更新作罢, 那时上一轮的 `.old` 是用户唯一的回退副本, 不该被清掉。
    remove_suffixed(core_dir, artifacts, OLD_SUFFIX);

    // 本轮已改名成 `.old` 的文件, 中途失败时按相反顺序恢复
    let mut renamed: Vec<(&str, PathBuf)> = Vec::new();

    for name in &staged {
        let dest = core_dir.join(name);
        if dest.exists() {
            let old = core_dir.join(format!("{name}.{OLD_SUFFIX}"));
            if let Err(e) = rename_with_retry(&dest, &old) {
                return Err(replace_failed(core_dir, &renamed, name, "备份", &e));
            }
            renamed.push((name.as_str(), old));
        }
        let tmp_path = core_dir.join(format!("{name}.{NEW_SUFFIX}"));
        if let Err(e) = rename_with_retry(&tmp_path, &dest) {
            return Err(replace_failed(core_dir, &renamed, name, "替换", &e));
        }
        info!("已更新 {name}");
    }

    Ok(())
}

/// 替换中途失败时先回滚, 再组装错误信息。
///
/// 失败点如果落在"旧文件已挪走、新文件还没到位"之间, 核心目录里就没有可用的
/// 核心文件了, 界面上会显示成未安装。所以必须把已挪走的旧文件放回去; 万一连
/// 恢复都失败, 错误信息要指名道姓地给出需要手工处理的路径, 而不是只说一句
/// "更新失败"。
fn replace_failed(core_dir: &Path, renamed: &[(&str, PathBuf)], name: &str, action: &str, err: &io::Error) -> AppError {
    let unrecovered = rollback(core_dir, renamed);
    if unrecovered.is_empty() {
        return AppError::Msg(format!("{action} {name} 失败: {err} (已恢复到更新前的版本)"));
    }

    let paths: Vec<String> = unrecovered
        .iter()
        .map(|n| core_dir.join(format!("{n}.{OLD_SUFFIX}")).display().to_string())
        .collect();
    AppError::Msg(format!(
        "{action} {name} 失败: {err}; 以下文件未能自动恢复, 请手动去掉 .{OLD_SUFFIX} 后缀: {}",
        paths.join(", ")
    ))
}

/// 把本轮已改名成 `.old` 的文件恢复回原名, 返回未能恢复的文件名。
///
/// 按与替换相反的顺序恢复。已经换成新版的文件也会被还原, 因此回滚成功后整个
/// 核心目录回到更新前的状态, 不会留下版本错配的组合。
fn rollback(core_dir: &Path, renamed: &[(&str, PathBuf)]) -> Vec<String> {
    let mut unrecovered = Vec::new();
    for (name, old) in renamed.iter().rev() {
        match rename_with_retry(old, &core_dir.join(name)) {
            Ok(()) => info!("已恢复 {name}"),
            Err(e) => {
                error!("恢复 {name} 失败: {e}");
                unrecovered.push((*name).to_string());
            }
        }
    }
    unrecovered
}

/// 删除清单内文件的指定后缀副本。
fn remove_suffixed(core_dir: &Path, artifacts: &CoreArtifacts, suffix: &str) {
    for name in artifacts.required.iter().chain(artifacts.optional) {
        let path = core_dir.join(format!("{name}.{suffix}"));
        if !path.exists() {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => debug!("已清理 {}", path.display()),
            Err(e) => warn!("清理 {} 失败: {e}", path.display()),
        }
    }
}

// ═══════════════════════════════════════════════
// 规则集更新
// ═══════════════════════════════════════════════

/// 检查并更新规则集文件。
///
/// 遍历 `ruleset.entries`, 对每个规则集:
/// 1. 检查 `last_update` 是否超过 `interval_days` 天
/// 2. 下载 sha256sum 文件, 解析出期望哈希值
/// 3. 下载 dat 文件到临时位置, 校验 SHA256
/// 4. 校验通过后移动到 `xray_core/{name}.dat`
///
/// 返回成功更新的规则集名称列表 (供调用方更新 `last_update`) 。
pub fn update_ruleset(
    exe_dir: &Path,
    ruleset: &crate::settings::Ruleset,
    max_retries: u32,
    delay_secs: u64,
) -> Vec<String> {
    // interval_days 由 Settings::validate 限制在上限内; saturating_mul 是
    // 针对绕过校验直接调用本函数的兜底, 避免溢出 (debug 构建下会 panic) 。
    let interval_secs = ruleset.interval_days.saturating_mul(86400);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut updated_names = Vec::new();

    for (name, entry) in &ruleset.entries {
        if let Some(last) = entry.last_update
            && now.saturating_sub(last) < interval_secs
        {
            debug!("[ruleset] {name}: 未超过更新间隔, 跳过");
            continue;
        }

        info!("[ruleset] {name}: 开始更新");

        let expected_hash = match download_and_parse_sha256sum(&entry.sha256sum, max_retries, delay_secs) {
            Ok(hash) => hash,
            Err(e) => {
                warn!("[ruleset] {name}: 获取 SHA256 校验和失败: {e}");
                continue;
            }
        };

        let dat_path = Core::Xray.core_dir(exe_dir).join(format!("{name}.dat"));
        let tmp_path = Core::Xray.core_dir(exe_dir).join(format!("{name}.dat.tmp"));

        match download_ruleset_with_retry(&entry.dat, &tmp_path, &expected_hash, max_retries, delay_secs) {
            Ok(_) => {
                if let Err(e) = fs::rename(&tmp_path, &dat_path) {
                    warn!("[ruleset] {name}: 移动文件失败: {e}");
                    let _ = fs::remove_file(&tmp_path);
                    continue;
                }
                info!("[ruleset] {name}: 更新成功");
                updated_names.push(name.clone());
            }
            Err(e) => {
                warn!("[ruleset] {name}: 下载失败: {e}");
                let _ = fs::remove_file(&tmp_path);
            }
        }
    }

    updated_names
}

/// 带重试地下载 sha256sum 文件并解析出哈希值。
///
/// 文件格式: `<hash>  <filename>`
fn download_and_parse_sha256sum(url: &str, max_retries: u32, delay_secs: u64) -> Result<String, AppError> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            debug!("SHA256 校验和下载第 {attempt}/{max_retries} 次重试, 等待 {delay_secs}s...");
            std::thread::sleep(std::time::Duration::from_secs(delay_secs));
        }

        debug!("下载 SHA256 校验和: {url}");
        let body = match get_text(url) {
            Ok(b) => b,
            Err(e) => {
                warn!("SHA256 校验和获取失败 (第 {attempt}/{max_retries} 次): {e}");
                continue;
            }
        };

        match parse_sha256sum(&body) {
            Some(hash) => {
                debug!("SHA256 校验和获取成功: {hash}");
                return Ok(hash);
            }
            None => {
                warn!("SHA256 校验和解析失败 (第 {attempt}/{max_retries} 次): 内容格式无效");
                continue;
            }
        }
    }

    Err(AppError::Msg("获取 SHA256 校验和失败, 已达到最大重试次数".into()))
}

/// 解析 sha256sum 文件内容, 返回哈希值。
///
/// 格式: `<hash>  <filename>`
fn parse_sha256sum(content: &str) -> Option<String> {
    let hash = content.split_whitespace().next()?;
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hash.to_lowercase())
    } else {
        None
    }
}

/// 带重试的 dat 文件下载。
///
/// 下载到临时文件后校验 SHA256, 下载失败或校验不匹配均删除并重试。
fn download_ruleset_with_retry(
    url: &str,
    dest: &Path,
    expected_hash: &str,
    max_retries: u32,
    delay_secs: u64,
) -> Result<(), AppError> {
    for attempt in 1..=max_retries {
        if attempt > 1 {
            debug!("dat 文件下载第 {attempt}/{max_retries} 次重试, 等待 {delay_secs}s...");
            std::thread::sleep(std::time::Duration::from_secs(delay_secs));
        }

        if let Err(e) = download_file(url, dest) {
            warn!("dat 文件下载失败 (第 {attempt}/{max_retries} 次): {e}");
            let _ = fs::remove_file(dest);
            continue;
        }

        let actual = match sha256_file(dest) {
            Ok(h) => h,
            Err(e) => {
                warn!("SHA256 计算失败 (第 {attempt}/{max_retries} 次): {e}");
                let _ = fs::remove_file(dest);
                continue;
            }
        };
        if actual.eq_ignore_ascii_case(expected_hash) {
            info!("SHA256 校验通过: {actual}");
            return Ok(());
        }
        warn!("SHA256 校验失败: expected={expected_hash}, actual={actual}");
        let _ = fs::remove_file(dest);
    }

    Err(AppError::Msg("下载文件校验失败, 已达到最大重试次数".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_version_simple() {
        assert_eq!(extract_version("sing-box version 1.13.13"), Some("1.13.13".into()));
        assert_eq!(extract_version("Xray 24.12.18"), Some("24.12.18".into()));
        assert_eq!(extract_version("v1.0.0"), Some("1.0.0".into()));
    }

    #[test]
    fn test_extract_version_edge_cases() {
        assert_eq!(extract_version("no version here"), None);
        assert_eq!(extract_version("123"), None);
        assert_eq!(extract_version("version 1.2.3-beta"), Some("1.2.3".into()));
        assert_eq!(extract_version("1.2.3.4"), Some("1.2.3.4".into()));
    }

    #[test]
    fn test_is_newer_true() {
        assert!(is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("1.0.0", "1.1.0"));
        assert!(is_newer("1.0.0", "2.0.0"));
        assert!(is_newer("1.0.9", "1.0.10"));
        assert!(is_newer("0.0.0", "1.0.0"));
    }

    #[test]
    fn test_is_newer_false_equal() {
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("2.0.0", "2.0.0"));
    }

    #[test]
    fn test_is_newer_false_older() {
        assert!(!is_newer("2.0.0", "1.0.0"));
        assert!(!is_newer("1.0.1", "1.0.0"));
    }

    #[test]
    fn test_is_newer_different_lengths() {
        assert!(is_newer("1.0", "1.0.1"));
        assert!(!is_newer("1.0.1", "1.0"));
    }

    #[test]
    fn test_is_newer_malformed_segments() {
        // filter_map 静默丢弃非数字段, "1.0.beta" 等价于 "1.0"
        assert!(is_newer("1.0.beta", "1.0.1"));
        // 空段被丢弃, "1..0" 等价于 "1.0"
        assert!(!is_newer("1..0", "1.0.0"));
        // 全部为空段, 视为空版本 (全 0)
        assert!(!is_newer("..", "0.0.0"));
        // 超出 u32 范围的段被丢弃, "4294967296.0" 等价于 "0"
        assert!(is_newer("4294967296.0", "1.0.0"));
        // 空字符串视为全 0
        assert!(!is_newer("", ""));
    }

    #[test]
    fn test_sha256_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_file_abc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 数据量刻意超过 HASH_BUFFER_SIZE, 覆盖多次 read 的累积路径。
    #[test]
    fn test_sha256_file_large_multichunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let data = vec![0xABu8; 200_000];
        assert!(data.len() > HASH_BUFFER_SIZE, "样本必须跨多个缓冲区");
        fs::write(&path, &data).unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "1bd168fa4e29a8a1af90173db63749b296c7f417c23487dd03ebf21d6ed663a6"
        );
    }

    #[test]
    fn test_to_hex() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn test_sha256_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.bin");
        assert!(sha256_file(&path).is_err());
    }

    fn write_test_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write as _;
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for &(name, data) in entries {
            zip.start_file(name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    /// 测试用的提取清单, 结构与真实核心一致: 一个必需 exe 加一个可选 dll。
    const TEST_ARTIFACTS: CoreArtifacts = CoreArtifacts {
        required: &["core.exe"],
        optional: &["extra.dll"],
    };

    fn extract(zip_path: &Path, core_dir: &Path) -> Result<(), AppError> {
        extract_artifacts(zip_path, core_dir, &TEST_ARTIFACTS)
    }

    #[test]
    fn test_extract_artifacts_only_takes_wanted_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[
                ("core.exe", &b"exe"[..]),
                ("extra.dll", &b"dll"[..]),
                ("geoip.dat", &b"official"[..]),
                ("LICENSE", &b"license"[..]),
                ("README.md", &b"readme"[..]),
            ],
        );
        let core_dir = dir.path().join("core");

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "exe");
        assert_eq!(fs::read_to_string(core_dir.join("extra.dll")).unwrap(), "dll");
        for unwanted in ["geoip.dat", "LICENSE", "README.md"] {
            assert!(!core_dir.join(unwanted).exists(), "{unwanted} 不该被提取");
        }
    }

    /// 整件事的目的: 用户自己配置的规则集 dat 和运行时生成的文件必须留下。
    #[test]
    fn test_extract_artifacts_keeps_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[("core.exe", &b"new-exe"[..]), ("geoip.dat", &b"official"[..])],
        );
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("geoip.dat"), b"user-custom").unwrap();
        fs::write(core_dir.join("cache.db"), b"runtime").unwrap();

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(
            fs::read_to_string(core_dir.join("geoip.dat")).unwrap(),
            "user-custom",
            "用户配置的规则集被官方版覆盖了"
        );
        assert_eq!(fs::read_to_string(core_dir.join("cache.db")).unwrap(), "runtime");
    }

    #[test]
    fn test_extract_artifacts_keeps_replaced_file_as_old() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("core.exe", &b"new-exe"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("core.exe"), b"old-exe").unwrap();

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "new-exe");
        assert_eq!(
            fs::read_to_string(core_dir.join("core.exe.old")).unwrap(),
            "old-exe",
            "旧文件应保留为 .old 供回退"
        );
        assert!(!core_dir.join("core.exe.new").exists(), "临时文件应已改名到位");
    }

    /// `.old` 留到下一次更新开始时才清理, `.new` 残留也一并清掉。
    #[test]
    fn test_extract_artifacts_clears_leftovers_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("core.exe", &b"v3"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("core.exe"), b"v2").unwrap();
        fs::write(core_dir.join("core.exe.old"), b"v1").unwrap();
        fs::write(core_dir.join("extra.dll.new"), b"stale").unwrap();

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "v3");
        assert_eq!(
            fs::read_to_string(core_dir.join("core.exe.old")).unwrap(),
            "v2",
            ".old 应是本次被替换的版本, 不是上一轮的"
        );
        assert!(!core_dir.join("extra.dll.new").exists(), "残留的 .new 应被清理");
    }

    #[test]
    fn test_extract_artifacts_missing_required_keeps_core_dir_intact() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("extra.dll", &b"dll"[..]), ("README.md", &b"readme"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("core.exe"), b"current").unwrap();
        fs::write(core_dir.join("extra.dll"), b"current-dll").unwrap();

        let err = extract(&zip_path, &core_dir).unwrap_err();
        assert!(err.to_string().contains("core.exe"), "错误应指出缺哪个文件: {err}");

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "current");
        assert_eq!(
            fs::read_to_string(core_dir.join("extra.dll")).unwrap(),
            "current-dll",
            "必需文件缺失时不应替换任何文件"
        );
        assert!(!core_dir.join("extra.dll.new").exists(), "失败后应清理临时文件");
    }

    /// 本次更新失败时, 上一轮的 `.old` 是用户唯一的回退副本, 不能被清掉。
    #[test]
    fn test_extract_artifacts_missing_required_keeps_old_backup() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("extra.dll", &b"dll"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("core.exe"), b"current").unwrap();
        fs::write(core_dir.join("core.exe.old"), b"prev").unwrap();

        extract(&zip_path, &core_dir).unwrap_err();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "current");
        assert_eq!(
            fs::read_to_string(core_dir.join("core.exe.old")).unwrap(),
            "prev",
            "更新没成功就不该清掉上一轮的回退副本"
        );
    }

    /// 替换中途失败时, 已经挪走的旧文件必须回到原位, 否则核心目录里会缺文件,
    /// 界面上表现为"未安装"。
    #[test]
    fn test_rollback_restores_renamed_files() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path();
        fs::write(core_dir.join("core.exe.old"), b"old-exe").unwrap();
        fs::write(core_dir.join("extra.dll.old"), b"old-dll").unwrap();
        // extra.dll 已经换成新版, 回滚要把它一起还原, 不留版本错配的组合
        fs::write(core_dir.join("extra.dll"), b"new-dll").unwrap();

        let renamed = [
            ("extra.dll", core_dir.join("extra.dll.old")),
            ("core.exe", core_dir.join("core.exe.old")),
        ];
        assert!(rollback(core_dir, &renamed).is_empty(), "应全部恢复成功");

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "old-exe");
        assert_eq!(fs::read_to_string(core_dir.join("extra.dll")).unwrap(), "old-dll");
        assert!(!core_dir.join("core.exe.old").exists());
        assert!(!core_dir.join("extra.dll.old").exists());
    }

    /// 恢复失败的文件名必须报告出去, 错误信息靠它提示用户手动处理。
    #[test]
    fn test_rollback_reports_unrecovered() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path();
        let renamed = [("missing.exe", core_dir.join("missing.exe.old"))];

        assert_eq!(rollback(core_dir, &renamed), ["missing.exe".to_string()]);
    }

    /// 端到端覆盖回滚: 后一个文件替换失败时, 前一个必须还原, 核心目录整体回到
    /// 更新前的状态, 不留下"新 exe + 旧 dll"的组合。
    ///
    /// 用目录占住改名的目标来制造失败: Windows 上 MoveFileEx 只能覆盖文件, 目标
    /// 是已存在的目录时必定失败。真实场景里对应的是句柄占用, 但那要绕过 std 的
    /// 共享标志自己 CreateFileW, 而这里只关心失败之后的恢复行为。
    ///
    /// 这个用例会走满改名重试, 因此比其他用例慢几秒。
    #[test]
    fn test_extract_artifacts_rolls_back_when_replace_fails() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[("core.exe", &b"new-exe"[..]), ("extra.dll", &b"new-dll"[..])],
        );
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("core.exe"), b"old-exe").unwrap();
        fs::write(core_dir.join("extra.dll"), b"old-dll").unwrap();
        fs::create_dir_all(core_dir.join(format!("extra.dll.{OLD_SUFFIX}"))).unwrap();

        let err = extract(&zip_path, &core_dir).unwrap_err();
        assert!(err.to_string().contains("extra.dll"), "错误应指出哪个文件失败: {err}");
        assert!(err.to_string().contains("已恢复"), "回滚成功后要告知用户: {err}");

        assert_eq!(
            fs::read_to_string(core_dir.join("core.exe")).unwrap(),
            "old-exe",
            "已经换成新版的文件必须还原"
        );
        assert_eq!(fs::read_to_string(core_dir.join("extra.dll")).unwrap(), "old-dll");
        assert!(!core_dir.join(format!("core.exe.{NEW_SUFFIX}")).exists());
        assert!(!core_dir.join(format!("extra.dll.{NEW_SUFFIX}")).exists());
    }

    #[test]
    fn test_extract_artifacts_missing_optional_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("core.exe", &b"exe"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("extra.dll"), b"kept").unwrap();

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "exe");
        assert_eq!(
            fs::read_to_string(core_dir.join("extra.dll")).unwrap(),
            "kept",
            "zip 里没有可选文件时应保留原有的"
        );
    }

    /// 按文件名匹配, 因此 sing-box 那种带版本号的顶层目录不需要额外剥离。
    #[test]
    fn test_extract_artifacts_matches_by_file_name_in_nested_dir() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[
                ("sing-box-1.2.3-windows-amd64/core.exe", &b"exe"[..]),
                ("sing-box-1.2.3-windows-amd64/LICENSE", &b"license"[..]),
            ],
        );
        let core_dir = dir.path().join("core");

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "exe");
        assert!(!core_dir.join("sing-box-1.2.3-windows-amd64").exists());
        assert!(!core_dir.join("LICENSE").exists());
    }

    /// 路径穿越条目即使文件名在清单里, 也必须被拒绝。
    #[test]
    fn test_extract_artifacts_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("../core.exe", &b"pwned"[..]), ("core.exe", &b"good"[..])]);
        let core_dir = dir.path().join("core");

        extract(&zip_path, &core_dir).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("core.exe")).unwrap(), "good");
        assert!(
            !dir.path().join("core.exe").exists(),
            "路径穿越条目被写到了 core_dir 之外"
        );
    }
}
