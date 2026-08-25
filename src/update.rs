//! 核心更新逻辑。
//!
//! 通过 GitHub Releases API 检查 sing-box / xray 的最新版本,
//! 与本地版本比较后决定是否下载更新。支持 CDN 代理、SHA256 校验、
//! 自动重试, 下载完成后从 zip 中提取 exe 并替换。

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::error::AppError;

/// GitHub API 要求的 User-Agent 头, 缺少会返回 403。
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Edg/149.0.0.0";

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
    let exe_dir = exe_dir.to_path_buf();
    update_sing_box(&exe_dir, gh_proxy_url, max_retries, delay_secs)?;
    update_xray(&exe_dir, gh_proxy_url, max_retries, delay_secs)
}

/// 检查并更新 sing-box。
pub fn update_sing_box(exe_dir: &Path, gh_proxy_url: &str, max_retries: u32, delay_secs: u64) -> Result<(), AppError> {
    let exe_path = exe_dir.join("sing-box_core").join("sing-box.exe");

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
        return Ok(());
    }

    let core_dir = exe_dir.join("sing-box_core");
    let nested_prefix = format!("sing-box-{}-windows-{}/", remote_ver, SINGBOX_ARCH_SUFFIX);
    backup_and_extract(&zip_path, &core_dir, Some(&nested_prefix))?;

    let _ = fs::remove_file(&zip_path);
    info!("[sing-box] 更新完成 -> v{remote_ver}");
    crate::toast::show_toast_tagged("sing-box", "更新完成", tag);
    Ok(())
}

/// 检查并更新 xray。
pub fn update_xray(exe_dir: &Path, gh_proxy_url: &str, max_retries: u32, delay_secs: u64) -> Result<(), AppError> {
    let exe_path = exe_dir.join("xray_core").join("xray.exe");

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
        return Ok(());
    }

    let core_dir = exe_dir.join("xray_core");
    backup_and_extract(&zip_path, &core_dir, None)?;

    let _ = fs::remove_file(&zip_path);
    info!("[xray] 更新完成 -> v{remote_ver}");
    crate::toast::show_toast_tagged("xray", "更新完成", tag);
    Ok(())
}

/// 运行可执行文件的版本命令并从 stdout 提取版本号, 失败返回 "0.0.0"。
pub(crate) fn get_local_version(exe_path: &Path, version_arg: &str) -> String {
    let output = match Command::new(exe_path).arg(version_arg).output() {
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
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| AppError::Msg(format!("读取文件计算 SHA256 失败: {e}")))?;
        if n == 0 {
            break;
        }
        Digest::update(&mut hasher, &buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 备份核心目录并从 zip 解压全部内容。
///
/// 1. 删除 `{core_dir}_backup` (如存在)
/// 2. 重命名 `{core_dir}` → `{core_dir}_backup`
/// 3. 解压 zip 全部内容到 `{core_dir}`
///
/// `strip_prefix` 为 Some 时, 跳过 zip 中以此开头的顶层目录 (用于 sing-box 的嵌套目录结构) 。
fn backup_and_extract(zip_path: &Path, core_dir: &Path, strip_prefix: Option<&str>) -> Result<(), AppError> {
    let backup_dir = PathBuf::from(format!("{}_backup", core_dir.display()));

    if backup_dir.exists() {
        debug!("删除旧备份: {}", backup_dir.display());
        fs::remove_dir_all(&backup_dir).map_err(|e| AppError::Msg(format!("删除旧备份失败: {e}")))?;
    }

    if core_dir.exists() {
        debug!("备份: {} -> {}", core_dir.display(), backup_dir.display());
        fs::rename(core_dir, &backup_dir).map_err(|e| AppError::Msg(format!("备份目录失败: {e}")))?;
    }

    fs::create_dir_all(core_dir).map_err(|e| AppError::Msg(format!("创建核心目录失败: {e}")))?;

    let file = fs::File::open(zip_path).map_err(|e| AppError::Msg(format!("打开 zip 文件失败: {e}")))?;
    let reader = BufReader::new(file);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| AppError::Msg(format!("解析 zip 文件失败: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::Msg(format!("读取 zip 条目失败: {e}")))?;

        // enclosed_name 会拒绝绝对路径、含 NULL 字节, 以及用 .. 逃出目标目录的条目。
        //
        // 不能改回 name() 自行拼接后用 starts_with 判断: Path::starts_with 是按
        // 组件的词法比较, 不解析 .., 因此 core_dir/../evil 会被判定为 core_dir
        // 的子路径, 检查形同虚设。
        let Some(entry_path) = entry.enclosed_name() else {
            warn!("zip 条目路径不安全, 跳过: {}", entry.name());
            continue;
        };

        let rel_path = match strip_prefix {
            Some(prefix) => match entry_path.strip_prefix(prefix) {
                Ok(rest) => rest.to_path_buf(),
                Err(_) => continue,
            },
            None => entry_path,
        };

        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let out_path = core_dir.join(&rel_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| AppError::Msg(format!("创建目录失败: {e}")))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| AppError::Msg(format!("创建父目录失败: {e}")))?;
            }
            let mut out = fs::File::create(&out_path).map_err(|e| AppError::Msg(format!("创建文件失败: {e}")))?;
            io::copy(&mut entry, &mut out).map_err(|e| AppError::Msg(format!("解压文件失败: {e}")))?;
        }
    }

    debug!("解压完成: {} -> {}", zip_path.display(), core_dir.display());
    Ok(())
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

        let dat_path = exe_dir.join("xray_core").join(format!("{name}.dat"));
        let tmp_path = exe_dir.join("xray_core").join(format!("{name}.dat.tmp"));

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

    #[test]
    fn test_sha256_file_large_multichunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let data = vec![0xABu8; 20000];
        fs::write(&path, &data).unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "1b53c5e8138cf85261885e5efbd49452254ad6ad365603d05fc7776d5eee93c0"
        );
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

    #[test]
    fn test_backup_and_extract_no_strip_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("a.txt", &b"hello"[..]), ("sub/b.txt", &b"world"[..])]);
        let core_dir = dir.path().join("core");

        backup_and_extract(&zip_path, &core_dir, None).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("a.txt")).unwrap(), "hello");
        assert_eq!(fs::read_to_string(core_dir.join("sub/b.txt")).unwrap(), "world");
    }

    #[test]
    fn test_backup_and_extract_strip_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[
                ("sing-box-1.2.3-windows-amd64/sing-box.exe", &b"exe"[..]),
                ("sing-box-1.2.3-windows-amd64/sub/file.txt", &b"data"[..]),
            ],
        );
        let core_dir = dir.path().join("core");

        backup_and_extract(&zip_path, &core_dir, Some("sing-box-1.2.3-windows-amd64/")).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("sing-box.exe")).unwrap(), "exe");
        assert_eq!(fs::read_to_string(core_dir.join("sub/file.txt")).unwrap(), "data");
        assert!(!core_dir.join("sing-box-1.2.3-windows-amd64").exists());
    }

    /// 路径穿越条目必须被跳过, 不能写到 core_dir 之外。
    #[test]
    fn test_backup_and_extract_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[("../evil.txt", &b"pwned"[..]), ("good.txt", &b"ok"[..])],
        );
        let core_dir = dir.path().join("core");

        backup_and_extract(&zip_path, &core_dir, None).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("good.txt")).unwrap(), "ok");
        assert!(
            !dir.path().join("evil.txt").exists(),
            "路径穿越条目被写到了 core_dir 之外"
        );
    }

    /// 剥离顶层目录时, 穿越到目标目录之外的条目同样必须被跳过。
    #[test]
    fn test_backup_and_extract_rejects_traversal_with_strip_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(
            &zip_path,
            &[
                ("sing-box-1.2.3-windows-amd64/sing-box.exe", &b"exe"[..]),
                ("sing-box-1.2.3-windows-amd64/../../evil.txt", &b"pwned"[..]),
            ],
        );
        let core_dir = dir.path().join("core");

        backup_and_extract(&zip_path, &core_dir, Some("sing-box-1.2.3-windows-amd64/")).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("sing-box.exe")).unwrap(), "exe");
        assert!(!dir.path().join("evil.txt").exists());
        assert!(!dir.path().parent().unwrap().join("evil.txt").exists());
    }

    #[test]
    fn test_backup_and_extract_backs_up_existing() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("core.zip");
        write_test_zip(&zip_path, &[("new.txt", &b"new"[..])]);
        let core_dir = dir.path().join("core");
        fs::create_dir_all(&core_dir).unwrap();
        fs::write(core_dir.join("old.txt"), b"old").unwrap();

        backup_and_extract(&zip_path, &core_dir, None).unwrap();

        assert_eq!(fs::read_to_string(core_dir.join("new.txt")).unwrap(), "new");
        assert!(!core_dir.join("old.txt").exists());

        let backup_dir = dir.path().join("core_backup");
        assert_eq!(fs::read_to_string(backup_dir.join("old.txt")).unwrap(), "old");
    }
}
