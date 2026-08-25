//! 子进程生命周期管理、TUN 接口配置与 DNS 缓存刷新。
//!
//! 负责 sing-box / xray 子进程的启停控制、TUN 接口名随机化 (`tun-` 前缀) 、
//! 孤立 WinTUN 设备节点清理、网络注册表清理、以及 DNS 缓存刷新。

use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::ptr::null;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use serde_json::Value;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{CREATE_NO_WINDOW, OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_Locate_DevNodeW, CR_SUCCESS,
    DN_HAS_PROBLEM, DN_STARTED,
};

use crate::dns;
use crate::error::AppError;
use crate::state::{self};
use crate::update;

// dnsapi.dll 导入, 用于刷新系统 DNS 缓存。
#[link(name = "dnsapi")]
unsafe extern "system" {
    fn DnsFlushResolverCache();
}

/// 创建不显示控制台窗口的子进程 Command。
pub fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW.0);
    command
}

// ═══════════════════════════════════════════════
// 进程启动
// ═══════════════════════════════════════════════

/// 将子进程的 stderr 重定向到日志输出。
fn forward_stderr(child: &mut std::process::Child, label: &str) {
    if let Some(stderr) = child.stderr.take() {
        let label = label.to_owned();
        let name = format!("stderr-{label}");
        let _ = std::thread::Builder::new().name(name).spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                warn!("[{label}] {line}");
            }
        });
    }
}

/// 启动 sing-box 子进程。启动前随机化 TUN 接口名。
pub fn start_sing_box_at(exe_dir: &Path) -> Result<(), AppError> {
    let core = state::Core::SingBox;
    let exe = core.exe_path(exe_dir);
    let config = core.active_config(exe_dir);

    state::ensure_exists(&exe)?;
    state::ensure_exists(&config)?;
    randomize_sing_box_tun_name(&config)?;

    info!("启动 sing-box");
    let mut child = hidden_command(exe)
        .args(["run", "-D"])
        .arg(core.core_dir(exe_dir))
        .arg("-c")
        .arg(config)
        .current_dir(exe_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Msg(format!("启动 sing-box 失败: {e}")))?;
    forward_stderr(&mut child, core.label());
    if let Some(mut app) = state::app_state_mut() {
        app.set_child(core, child);
    }

    Ok(())
}

/// 启动 xray 子进程。启动前随机化 TUN 接口名。
pub fn start_xray_at(exe_dir: &Path) -> Result<(), AppError> {
    let core = state::Core::Xray;
    let exe = core.exe_path(exe_dir);
    let config = core.active_config(exe_dir);

    state::ensure_exists(&exe)?;
    state::ensure_exists(&config)?;

    // 一次性读取并解析配置, 用于 TUN 检测和接口名随机化
    let text = fs::read_to_string(&config).map_err(|e| AppError::Msg(format!("读取 xray 配置失败: {e}")))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| AppError::Msg(format!("解析 xray 配置失败: {e}")))?;

    // 检测是否有 TUN inbound
    let has_tun = json
        .get("inbounds")
        .and_then(Value::as_array)
        .map(|inbounds| {
            inbounds
                .iter()
                .any(|inbound| inbound.get("protocol").and_then(Value::as_str) == Some("tun"))
        })
        .unwrap_or(false);

    if has_tun {
        info!("xray 配置包含 TUN 模式");
        randomize_xray_tun_name(&config, &text, &json)?;
    }

    info!("启动 xray");
    let mut child = hidden_command(exe)
        .args(["run", "-c"])
        .arg(config)
        .current_dir(exe_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::Msg(format!("启动 xray 失败: {e}")))?;

    if has_tun {
        dns::set_physical_dns_to_local();
    }

    forward_stderr(&mut child, core.label());
    if let Some(mut app) = state::app_state_mut() {
        app.set_child(core, child);
    }

    Ok(())
}

// ═══════════════════════════════════════════════
// 进程停止
// ═══════════════════════════════════════════════

/// 终止所有已知子进程并刷新 DNS。
pub fn stop_all() -> Result<(), AppError> {
    stop_processes(&state::Core::ALL)
}

/// 按进程名终止两个核心并恢复网络状态, 全程不触碰 `AppState`。
///
/// 供 panic 钩子使用。`stop_all` 会取 `AppState` 的 Mutex 来拿 Child 句柄, 而
/// panic 完全可能发生在持锁期间, 此时再取同一把锁就是自死锁。这里跳过 Child
/// 句柄, 只按名字终止, 因此不存在这个风险。
pub fn kill_cores_without_state() {
    for core in state::Core::ALL {
        kill_processes_by_name(core.exe_name());
    }
    dns::restore_dns_to_dhcp();
    flush_dns();
}

/// 终止指定核心。先通过保存的 Child 句柄直接 kill,
/// 再通过进程名枚举兜底 (覆盖其他来源启动的同名进程) 。
pub fn stop_processes(cores: &[state::Core]) -> Result<(), AppError> {
    // 阶段 1: 从 mutex 中取出 Child 句柄 (持锁时间极短)
    let children: Vec<(state::Core, Option<std::process::Child>)> = match state::app_state_mut() {
        Some(mut app) => cores.iter().map(|&core| (core, app.take_child(core))).collect(),
        None => cores.iter().map(|&core| (core, None)).collect(),
    };
    // 阶段 2: kill (锁已释放, 不阻塞其他线程)
    for (core, child) in children {
        if let Some(mut c) = child {
            info!("终止子进程: {}", core.exe_name());
            let _ = c.kill();
        }
    }
    // 阶段 3: 通过进程名枚举兜底 (覆盖其他来源启动的同名进程)
    for core in cores {
        info!("终止进程: {}", core.exe_name());
        kill_processes_by_name(core.exe_name());
    }
    // xray 的 TUN 模式会劫持物理网卡 DNS, 停止时必须恢复
    if cores.contains(&state::Core::Xray) {
        dns::restore_dns_to_dhcp();
    }
    flush_dns();
    Ok(())
}

/// 终止与 `exe_name` 匹配的所有进程。
fn kill_processes_by_name(exe_name: &str) {
    for pid in state::find_pids_by_name(exe_name) {
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                debug!("终止进程: {} (PID {})", exe_name, pid);
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
}

// ═══════════════════════════════════════════════
// 重启
// ═══════════════════════════════════════════════

/// 规则集更新是否正在进行。
static RULESET_UPDATING: AtomicBool = AtomicBool::new(false);

/// 在独立线程中启动规则集更新, 立即返回。
///
/// 刻意不占用菜单的忙标志: 更新会先等 5 秒网络就绪, 再串行下载几十 MB 的 dat
/// 文件, 整个过程可能持续几十秒甚至更久。挂在忙标志上会让用户切一次配置后长
/// 时间无法做任何操作, 而规则集是否更新完并不影响核心已经启动这一事实。
///
/// 用独立的标志防重入: 连续切换配置会重复触发, 两个线程会写同一批 .dat.tmp
/// 临时文件并互相覆盖。
fn spawn_ruleset_update() {
    let Some(guard) = state::FlagGuard::acquire(&RULESET_UPDATING) else {
        debug!("[ruleset] 已有更新在进行, 跳过本次触发");
        return;
    };

    let spawned = std::thread::Builder::new().name("bg-ruleset".into()).spawn(move || {
        let _guard = guard;
        run_ruleset_update();
    });

    // 线程创建失败时闭包连同 guard 一起在此 drop, 标志不会泄漏
    if let Err(e) = spawned {
        warn!("[ruleset] 创建更新线程失败: {e}");
    }
}

/// 执行规则集更新 (静默, 失败仅 warn) 。
///
/// 分两阶段获取锁: 下载前克隆数据释放锁, 下载完成后再获取锁更新 `last_update`,
/// 避免长时间持锁阻塞托盘菜单。
fn run_ruleset_update() {
    let (exe_dir, ruleset, max_retries, delay_secs) = {
        let app = match state::app_state() {
            Some(a) => a,
            None => return,
        };
        (
            app.exe_dir.clone(),
            app.settings.download.ruleset.clone(),
            app.settings.download.retry.max_retries,
            app.settings.download.retry.delay_secs,
        )
    };

    debug!("[ruleset] 等待 5 秒让网络就绪...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    let updated = update::update_ruleset(&exe_dir, &ruleset, max_retries, delay_secs);

    if !updated.is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(mut app) = state::app_state_mut() {
            for name in &updated {
                if let Some(entry) = app.settings.download.ruleset.entries.get_mut(name) {
                    entry.last_update = Some(now);
                }
            }
            if let Err(e) = app.settings.save(&exe_dir) {
                warn!("[ruleset] 保存配置失败: {e}");
            }
        }
    }
}

pub fn restart_all_at(exe_dir: &Path) -> Result<(), AppError> {
    stop_all()?;
    cleanup_orphaned_wintun();
    start_sing_box_at(exe_dir)?;
    start_xray_at(exe_dir)?;
    spawn_ruleset_update();
    Ok(())
}

pub fn restart_sing_box_at(exe_dir: &Path) -> Result<(), AppError> {
    stop_processes(&[state::Core::SingBox])?;
    cleanup_orphaned_wintun();
    start_sing_box_at(exe_dir)
}

pub fn restart_xray_at(exe_dir: &Path) -> Result<(), AppError> {
    stop_processes(&[state::Core::Xray])?;
    cleanup_orphaned_wintun();
    start_xray_at(exe_dir)?;
    spawn_ruleset_update();
    Ok(())
}

// ═══════════════════════════════════════════════
// TUN 管理
// ═══════════════════════════════════════════════

/// sing-box 配置中标识 TUN inbound 的文本片段。
const SING_BOX_TUN_PATTERN: &str = "\"type\": \"tun\"";

/// xray 配置中标识 TUN inbound 的文本片段。
const XRAY_TUN_PATTERN: &str = "\"protocol\": \"tun\"";

/// xray inbound 中 settings 块的起始文本片段。
const XRAY_SETTINGS_PATTERN: &str = "\"settings\": {";

/// 随机化 sing-box 配置中的 TUN 接口名。
///
/// sing-box TUN 适配器在 Windows 上以固定名称注册, 重启时如果旧适配器
/// 未完全释放会导致冲突。通过每次启动时生成带 `tun-` 前缀的随机名称来避免。
///
/// 使用字符串替换而非 JSON 序列化来保留原始配置的格式和注释。
/// 如果配置中没有 type=tun 的 inbound, 静默跳过 (用户可能不使用 TUN 功能) 。
/// 如果 tun inbound 缺少 interface_name 字段, 自动写入随机化后的值。
fn randomize_sing_box_tun_name(config_path: &Path) -> Result<(), AppError> {
    let text = fs::read_to_string(config_path).map_err(|e| AppError::Msg(format!("读取 sing-box 配置失败: {e}")))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| AppError::Msg(format!("解析 sing-box 配置失败: {e}")))?;
    let new_name = random_tun_name();

    // 找 tun inbound, 找不到直接跳过
    let tun_inbound = json.get("inbounds").and_then(Value::as_array).and_then(|inbounds| {
        inbounds
            .iter()
            .find(|inbound| inbound.get("type").and_then(Value::as_str) == Some("tun"))
    });

    let tun_inbound = match tun_inbound {
        Some(inbound) => inbound,
        None => {
            debug!("未发现 type=tun 的 inbound, 跳过 TUN 接口名随机化");
            return Ok(());
        }
    };

    // 有 interface_name → 替换
    if let Some(old_name) = tun_inbound.get("interface_name").and_then(Value::as_str) {
        if old_name == new_name {
            return Ok(());
        }
        info!("随机化 sing-tun 接口名: {old_name} -> {new_name}");
        let old_pattern = format!("\"interface_name\": \"{}\"", old_name);
        let new_pattern = format!("\"interface_name\": \"{}\"", new_name);
        let new_text = text.replacen(&old_pattern, &new_pattern, 1);
        return fs::write(config_path, new_text).map_err(|e| AppError::Msg(format!("写入 sing-box 配置失败: {e}")));
    }

    // 无 interface_name → 紧跟 "type": "tun" 之后插入
    //
    // 插入点取 pattern 末尾而非所在行的行尾, 原因有两个:
    // 1. 行尾插入依赖 "配置是多行缩进" 这一假设。压缩成单行的 JSON 会被写到
    //    对象外部, 而 tun 标记恰好位于末行且文件无尾随换行时还会索引越界。
    // 2. 逗号前置 (", \"key\": \"value\"") 后, 无论紧跟的是另一个字段还是
    //    对象结尾 }, 都不会产生尾随逗号。
    debug!("写入 sing-tun 接口名: {new_name}");
    let pos = text
        .find(SING_BOX_TUN_PATTERN)
        .ok_or(AppError::Msg("未在 sing-box.json 中找到 type=tun 的 inbound".into()))?;
    let mut new_text = text;
    new_text.insert_str(
        pos + SING_BOX_TUN_PATTERN.len(),
        &format!(", \"interface_name\": \"{new_name}\""),
    );
    fs::write(config_path, new_text).map_err(|e| AppError::Msg(format!("写入 sing-box 配置失败: {e}")))
}

/// 随机化 xray 配置中的 TUN 接口名。
///
/// xray 的 TUN inbound 使用 `"protocol": "tun"` 标识, 接口名位于
/// `settings.name` 字段 (如 `"name": "xray0"`) 。逻辑与
/// `randomize_sing_box_tun_name` 对称: 字符串替换保留原始格式。
///
/// 调用者负责保证配置中存在 TUN inbound, 并传入已读取的 `text` 和已解析的 `json`。
fn randomize_xray_tun_name(config_path: &Path, text: &str, json: &Value) -> Result<(), AppError> {
    let new_name = random_tun_name();

    // 定位 TUN inbound (调用者已保证 TUN 存在)
    let tun_inbound = json
        .get("inbounds")
        .and_then(Value::as_array)
        .and_then(|inbounds| {
            inbounds
                .iter()
                .find(|inbound| inbound.get("protocol").and_then(Value::as_str) == Some("tun"))
        })
        .ok_or_else(|| AppError::Msg("xray 配置中应存在 TUN inbound".into()))?;

    // 有 settings.name → 替换
    if let Some(old_name) = tun_inbound
        .get("settings")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
    {
        if old_name == new_name {
            return Ok(());
        }
        info!("随机化 Xray TUN 接口名: {old_name} -> {new_name}");
        let old_pattern = format!("\"name\": \"{}\"", old_name);
        let new_pattern = format!("\"name\": \"{}\"", new_name);
        let new_text = text.replacen(&old_pattern, &new_pattern, 1);
        return fs::write(config_path, new_text).map_err(|e| AppError::Msg(format!("写入 xray 配置失败: {e}")));
    }

    // 无 settings.name → 写入随机名。
    //
    // 与 sing-box 不同, xray 的接口名嵌在 settings 子对象里, 因此分两种情况:
    // 1. tun inbound 没有 settings 字段: 紧跟 "protocol": "tun" 之后插入整个
    //    settings 对象, 逗号前置, 不会产生尾随逗号。
    // 2. 有 settings 但缺 name: 紧跟 "settings": { 之后插入 name。插入点位于
    //    对象开头, 逗号只能后置, 所以空对象不能补逗号, 否则破坏 JSON。
    debug!("写入 Xray TUN 接口名: {new_name}");
    let (pattern, insert) = match tun_inbound.get("settings") {
        None => (
            XRAY_TUN_PATTERN,
            format!(", \"settings\": {{\"name\": \"{new_name}\"}}"),
        ),
        Some(settings) => {
            let obj = settings
                .as_object()
                .ok_or_else(|| AppError::Msg("xray.json 中 tun inbound 的 settings 不是对象".into()))?;
            let separator = if obj.is_empty() { "" } else { "," };
            (XRAY_SETTINGS_PATTERN, format!("\"name\": \"{new_name}\"{separator}"))
        }
    };
    let pos = text
        .find(pattern)
        .ok_or_else(|| AppError::Msg(format!("未在 xray.json 中找到 {pattern}")))?;
    let mut new_text = text.to_string();
    new_text.insert_str(pos + pattern.len(), &insert);
    fs::write(config_path, new_text).map_err(|e| AppError::Msg(format!("写入 xray 配置失败: {e}")))
}

/// TUN 接口名前缀, 用于在注册表中识别本程序创建的网络配置。
const TUN_PREFIX: &str = "tun-";

/// 生成带 `tun-` 前缀的随机 TUN 接口名 (如 `tun-0ad1f0`) 。
/// 使用 RandomState 对时间戳做哈希, 每次运行种子不同。
fn random_tun_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    let seed = std::hash::RandomState::new();
    let mut hasher = seed.build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    hasher.write_u128(nanos);
    format!("{TUN_PREFIX}{:06x}", hasher.finish() & 0xFF_FFFF)
}

/// 清理 Windows 网络配置注册表中 TUN 相关的子项。
///
/// 枚举 `NetworkList\Profiles` 和 `NetworkList\Signatures\Unmanaged`
/// 下的所有子项, 删除 `Description` 值以 `tun-` 开头的条目。
///
/// 需要管理员权限; 若权限不足会记录警告但不阻断启动流程。
pub fn cleanup_network_registry() {
    const PROFILES: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\NetworkList\Profiles";
    const SIGNATURES: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\NetworkList\Signatures\Unmanaged";

    info!("清理 TUN 相关网络注册表项...");
    clean_tun_entries(PROFILES);
    clean_tun_entries(SIGNATURES);
}

/// 枚举 `parent_path` 下的所有子项, 删除 `Description` 值以 `TUN_PREFIX` 开头的子项。
fn clean_tun_entries(parent_path: &str) {
    use windows_registry::LOCAL_MACHINE;

    let parent = match LOCAL_MACHINE.open(parent_path) {
        Ok(k) => k,
        Err(e) => {
            warn!("打开注册表键失败: {parent_path}: {e}");
            return;
        }
    };

    let subkeys: Vec<String> = match parent.keys() {
        Ok(iter) => iter.collect(),
        Err(e) => {
            warn!("枚举子项失败: {parent_path}: {e}");
            return;
        }
    };

    for name in subkeys {
        let subkey = match parent.open(&name) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let desc = match subkey.get_string("Description") {
            Ok(d) => d,
            Err(_) => continue,
        };
        if desc.starts_with(TUN_PREFIX) {
            let full = format!("{parent_path}\\{name}");
            match LOCAL_MACHINE.remove_tree(&full) {
                Ok(()) => debug!("已删除 TUN 注册表项: {full} (Description={desc})"),
                Err(e) => warn!("删除注册表项失败: {full}: {e}"),
            }
        }
    }
}

/// 清理孤立或异常的 WinTUN 设备节点。
///
/// sing-box TUN 模式依赖 WinTUN 驱动, 异常退出后可能残留无效设备节点,
/// 导致下次启动时 TUN 接口创建失败。此函数:
///
/// 1. 通过 `CM_Get_Device_ID_ListW` 枚举所有设备实例 ID
/// 2. 解析双 null 结尾的多字符串缓冲区
/// 3. 筛选包含 "WINTUN" 的设备
/// 4. 检查设备状态: 设备节点不存在 (CR_NO_SUCH_DEVNODE) 或未启动 / 有异常
/// 5. 通过 `pnputil /remove-device` 移除问题设备
/// 6. 最后执行 `pnputil /scan-devices` 重新扫描硬件
fn cleanup_orphaned_wintun() {
    const CR_NO_SUCH_DEVNODE: u32 = 0x0D;

    info!("检查孤立 WinTUN 设备...");
    let mut instance_ids: Vec<String> = Vec::new();

    unsafe {
        let mut size = 0u32;
        if CM_Get_Device_ID_List_SizeW(&mut size, null(), 0) != CR_SUCCESS {
            return;
        }
        if size == 0 {
            return;
        }

        let mut buffer: Vec<u16> = vec![0u16; size as usize];
        if CM_Get_Device_ID_ListW(null(), buffer.as_mut_ptr(), size, 0) != CR_SUCCESS {
            return;
        }

        let mut start = 0usize;
        while start < buffer.len() {
            let end = buffer[start..]
                .iter()
                .position(|&c| c == 0)
                .map(|p| start + p)
                .unwrap_or(buffer.len());
            if end == start {
                break;
            }
            let id = String::from_utf16_lossy(&buffer[start..end]);

            if id.to_uppercase().contains("WINTUN") {
                let mut dev_inst = 0u32;
                let wide_id = state::wide(&id);
                let locate_ret = CM_Locate_DevNodeW(&mut dev_inst, wide_id.as_ptr(), 0);

                if locate_ret == CR_NO_SUCH_DEVNODE {
                    instance_ids.push(id);
                } else if locate_ret == CR_SUCCESS {
                    let mut status = 0u32;
                    let mut problem = 0u32;
                    if CM_Get_DevNode_Status(&mut status, &mut problem, dev_inst, 0) == CR_SUCCESS
                        && ((status & DN_STARTED) == 0 || (status & DN_HAS_PROBLEM) != 0)
                    {
                        instance_ids.push(id);
                    }
                }
            }

            start = end + 1;
        }
    }

    if instance_ids.is_empty() {
        debug!("未发现孤立 WinTUN 设备");
        return;
    }
    debug!("发现 {} 个孤立 WinTUN 设备", instance_ids.len());

    for id in &instance_ids {
        debug!("移除孤立 WinTUN 设备: {id}");
        let result = hidden_command("pnputil").args(["/remove-device", id.as_str()]).status();
        match result {
            Ok(status) => debug!("pnputil /remove-device {id}: {status}"),
            Err(e) => warn!("pnputil /remove-device {id} 执行失败: {e}"),
        }
    }

    info!("扫描硬件变更 ({} 个设备已移除)", instance_ids.len());
    let _ = hidden_command("pnputil").arg("/scan-devices").status();
}

// ═══════════════════════════════════════════════
// DNS 缓存刷新
// ═══════════════════════════════════════════════

fn flush_dns() {
    unsafe { DnsFlushResolverCache() };
    info!("已刷新 DNS 缓存");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_tun_name() {
        let name1 = random_tun_name();
        let name2 = random_tun_name();
        assert!(name1.starts_with(TUN_PREFIX));
        assert_eq!(name1.len(), TUN_PREFIX.len() + 6);
        let hex_part = &name1[TUN_PREFIX.len()..];
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(name1, name2);
    }

    /// 写入配置并返回路径。
    fn write_config(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.json");
        fs::write(&path, content).unwrap();
        path
    }

    /// 读回配置并解析, 解析失败即说明改写产生了非法 JSON。
    fn read_json(path: &Path) -> Value {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("改写后的 JSON 非法: {e}\n{text}"))
    }

    fn sing_box_interface_name(json: &Value) -> &str {
        json["inbounds"][0]["interface_name"].as_str().expect("缺少接口名")
    }

    fn xray_tun_name(json: &Value) -> &str {
        json["inbounds"][0]["settings"]["name"].as_str().expect("缺少接口名")
    }

    fn randomize_xray(path: &Path) -> Result<(), AppError> {
        let text = fs::read_to_string(path).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        randomize_xray_tun_name(path, &text, &json)
    }

    #[test]
    fn test_sing_box_replaces_existing_interface_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "{\n  \"inbounds\": [\n    {\n      \"type\": \"tun\",\n      \"interface_name\": \"sing-tun\"\n    }\n  ]\n}\n",
        );

        randomize_sing_box_tun_name(&path).unwrap();

        let name = read_json(&path)["inbounds"][0]["interface_name"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(name.starts_with(TUN_PREFIX), "接口名未被随机化: {name}");
    }

    #[test]
    fn test_sing_box_inserts_missing_interface_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "{\n  \"inbounds\": [\n    {\n      \"type\": \"tun\",\n      \"tag\": \"in-tun\"\n    }\n  ]\n}\n",
        );

        randomize_sing_box_tun_name(&path).unwrap();

        let json = read_json(&path);
        assert!(sing_box_interface_name(&json).starts_with(TUN_PREFIX));
        // 原有字段不能被破坏
        assert_eq!(json["inbounds"][0]["tag"], "in-tun");
    }

    /// 回归: tun 标记位于末行且文件无尾随换行时, 行尾插入会索引越界 panic。
    #[test]
    fn test_sing_box_inserts_into_single_line_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "{\"inbounds\": [{\"type\": \"tun\"}]}");

        randomize_sing_box_tun_name(&path).unwrap();

        assert!(sing_box_interface_name(&read_json(&path)).starts_with(TUN_PREFIX));
    }

    #[test]
    fn test_sing_box_skips_config_without_tun() {
        let dir = tempfile::tempdir().unwrap();
        let original = "{\"inbounds\": [{\"type\": \"socks\"}]}";
        let path = write_config(&dir, original);

        randomize_sing_box_tun_name(&path).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn test_xray_replaces_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "{\n  \"inbounds\": [\n    {\n      \"protocol\": \"tun\",\n      \"settings\": {\n        \"name\": \"xray0\"\n      }\n    }\n  ]\n}\n",
        );

        randomize_xray(&path).unwrap();

        assert!(xray_tun_name(&read_json(&path)).starts_with(TUN_PREFIX));
    }

    /// 回归: settings 为空对象时后置逗号会产生尾随逗号, 破坏 JSON。
    #[test]
    fn test_xray_inserts_into_empty_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "{\"inbounds\": [{\"protocol\": \"tun\", \"settings\": {}}]}");

        randomize_xray(&path).unwrap();

        assert!(xray_tun_name(&read_json(&path)).starts_with(TUN_PREFIX));
    }

    #[test]
    fn test_xray_inserts_into_non_empty_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "{\n  \"inbounds\": [\n    {\n      \"protocol\": \"tun\",\n      \"settings\": {\n        \"mtu\": 1500\n      }\n    }\n  ]\n}\n",
        );

        randomize_xray(&path).unwrap();

        let json = read_json(&path);
        assert!(xray_tun_name(&json).starts_with(TUN_PREFIX));
        assert_eq!(json["inbounds"][0]["settings"]["mtu"], 1500);
    }

    /// tun inbound 完全没有 settings 字段时, 应插入整个 settings 对象,
    /// 而不是把 name 写进其他 inbound 的 settings 里。
    #[test]
    fn test_xray_inserts_whole_settings_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            &dir,
            "{\"inbounds\": [{\"protocol\": \"tun\"}, {\"protocol\": \"socks\", \"settings\": {\"udp\": true}}]}",
        );

        randomize_xray(&path).unwrap();

        let json = read_json(&path);
        assert!(xray_tun_name(&json).starts_with(TUN_PREFIX));
        // 另一个 inbound 的 settings 不能被污染
        assert_eq!(json["inbounds"][1]["settings"]["udp"], true);
        assert!(json["inbounds"][1]["settings"]["name"].is_null());
    }

    #[test]
    fn test_xray_rejects_non_object_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "{\"inbounds\": [{\"protocol\": \"tun\", \"settings\": []}]}");

        assert!(randomize_xray(&path).is_err());
    }
}
