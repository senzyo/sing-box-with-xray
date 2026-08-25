//! 共享类型定义、全局应用状态与通用工具函数。
//!
//! 各模块通过此文件访问全局单例 `AppState` 和公共工具函数,
//! 避免模块间循环依赖。

use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::warn;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

use crate::error::AppError;
use crate::settings;
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};

/// 两个代理核心。
///
/// 与核心绑定的文件名和目录名都集中在这里, 避免 "sing-box.exe" 这类字符串
/// 散落在各模块里被当成类型用 —— 那样拼错只会在运行时表现为"进程没找到"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Core {
    SingBox,
    Xray,
}

impl Core {
    /// 全部核心。顺序与 `CoreStates` 的字段一致, `core_states` 依赖这个顺序。
    pub const ALL: [Core; 2] = [Core::SingBox, Core::Xray];

    /// 日志与通知里使用的名称。
    pub fn label(self) -> &'static str {
        match self {
            Core::SingBox => "sing-box",
            Core::Xray => "xray",
        }
    }

    /// 可执行文件名。
    pub fn exe_name(self) -> &'static str {
        match self {
            Core::SingBox => "sing-box.exe",
            Core::Xray => "xray.exe",
        }
    }

    /// 核心的工作目录 (放可执行文件、规则集等) 。
    pub fn core_dir(self, exe_dir: &Path) -> PathBuf {
        let name = match self {
            Core::SingBox => "sing-box_core",
            Core::Xray => "xray_core",
        };
        exe_dir.join(name)
    }

    /// 可执行文件的完整路径。
    pub fn exe_path(self, exe_dir: &Path) -> PathBuf {
        self.core_dir(exe_dir).join(self.exe_name())
    }

    /// 当前生效的配置文件路径。
    pub fn active_config(self, exe_dir: &Path) -> PathBuf {
        let name = match self {
            Core::SingBox => "sing-box.json",
            Core::Xray => "xray.json",
        };
        exe_dir.join("configs").join(name)
    }

    /// 可切换配置所在的目录。
    pub fn config_dir(self, exe_dir: &Path) -> PathBuf {
        exe_dir.join("configs").join(self.label())
    }

    /// 更新核心时需要从 release zip 里取出的文件。
    pub fn artifacts(self) -> CoreArtifacts {
        match self {
            Core::SingBox => CoreArtifacts {
                required: &["sing-box.exe"],
                optional: &["libcronet.dll"],
            },
            Core::Xray => CoreArtifacts {
                required: &["xray.exe"],
                optional: &["wintun.dll", "geoip.dat", "geosite.dat"],
            },
        }
    }
}

/// 更新核心时从 release zip 中提取的文件清单。
///
/// 刻意只取这几个文件, 而不是解压整个包:
/// - xray 包里带的 `geoip.dat` / `geosite.dat` 是官方版, 全量解压会盖掉用户
///   自己配置的那套规则集 (settings.json 的 `download.ruleset`)
/// - `LICENSE`、`README.md`、`xray_no_window.*` 对本程序没用, 白占几十 MB
/// - 核心目录里还有 sing-box 运行时生成的 `cache.db`, 整体替换目录会连它
///   一起丢掉
pub struct CoreArtifacts {
    /// 必需文件, 任意一个在 zip 里找不到都视为更新失败。
    pub required: &'static [&'static str],
    /// 可选文件, 找不到只记录警告 —— 官方随时可能调整打包内容。
    pub optional: &'static [&'static str],
}

impl CoreArtifacts {
    /// 是否需要提取该文件名。
    pub fn wants(&self, file_name: &str) -> bool {
        self.required.contains(&file_name) || self.optional.contains(&file_name)
    }
}

#[derive(Clone)]
pub struct ConfigAction {
    pub core: Core,
    pub path: PathBuf,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    NotInstalled,
    NotRunning,
    Running,
}

/// 三个状态图标的 GDI 位图句柄, 0 表示加载失败。
#[derive(Clone, Copy, Default)]
pub struct StatusIcons {
    pub running: isize,
    pub not_running: isize,
    pub not_installed: isize,
}

impl StatusIcons {
    /// 取进程状态对应的位图句柄。
    pub fn handle_for(self, state: ProcessState) -> isize {
        match state {
            ProcessState::Running => self.running,
            ProcessState::NotRunning => self.not_running,
            ProcessState::NotInstalled => self.not_installed,
        }
    }

    /// 释放三个位图句柄。
    ///
    /// 刻意做成显式方法而不是 `Drop`: `AppState` 放在 `OnceLock` 里, 静态变量
    /// 的 Drop 永远不会执行, 写成 Drop 就成了不会运行的死代码。
    pub fn delete(self) {
        for handle in [self.running, self.not_running, self.not_installed] {
            if handle != 0 {
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(handle as *mut std::ffi::c_void));
                }
            }
        }
    }
}

/// 全局应用状态。
pub struct AppState {
    /// 可执行文件所在目录, 所有相对路径以此为基准。
    pub exe_dir: PathBuf,
    /// 托盘菜单里表示核心状态的图标。
    pub icons: StatusIcons,
    pub settings: settings::Settings,
    /// 子进程句柄, 用于直接 kill。
    pub child_sing_box: Option<Child>,
    pub child_xray: Option<Child>,
}

impl AppState {
    /// 取出核心的子进程句柄, 同时从状态中移除。
    pub fn take_child(&mut self, core: Core) -> Option<Child> {
        match core {
            Core::SingBox => self.child_sing_box.take(),
            Core::Xray => self.child_xray.take(),
        }
    }

    /// 记录核心的子进程句柄。
    pub fn set_child(&mut self, core: Core, child: Child) {
        match core {
            Core::SingBox => self.child_sing_box = Some(child),
            Core::Xray => self.child_xray = Some(child),
        }
    }
}

/// 全局应用状态, 通过 OnceLock + Mutex 实现线程安全的单例。
pub static APP: OnceLock<Mutex<AppState>> = OnceLock::new();

/// 布尔标志的 RAII 守卫: 获取成功即置位, Drop 时清零。
///
/// 用于"同一类操作同时只允许一个"的场景。手工配对 `store(false)` 时, 任何
/// 一条提前返回路径漏掉释放, 标志就永久置位、对应功能彻底失效; 交给 Drop
/// 之后, 提前返回、线程结束和闭包被丢弃都会自动释放。
pub struct FlagGuard(&'static AtomicBool);

impl FlagGuard {
    /// 尝试置位标志。已被占用时返回 `None`。
    pub fn acquire(flag: &'static AtomicBool) -> Option<Self> {
        if flag.swap(true, Ordering::SeqCst) {
            None
        } else {
            Some(Self(flag))
        }
    }
}

impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 菜单操作忙标志, 防止并发执行冲突操作。true 表示有操作正在执行。
///
/// 刻意不对外暴露, 只能通过 `acquire_busy` 占用。
static BUSY: AtomicBool = AtomicBool::new(false);

/// 占用菜单操作忙标志, 已有操作在执行时返回 `None`。
///
/// 托盘命令分发有近十条提前返回路径 (取不到 exe 目录、应用状态不可用、
/// 保存配置失败、创建后台线程失败……) , 所以必须用 RAII 而不是手工释放。
pub fn acquire_busy() -> Option<FlagGuard> {
    FlagGuard::acquire(&BUSY)
}

/// 获取只读应用状态。Mutex 中毒时恢复并继续使用。
pub fn app_state() -> Option<std::sync::MutexGuard<'static, AppState>> {
    Some(APP.get()?.lock().unwrap_or_else(|e| e.into_inner()))
}

/// 获取可变应用状态。语义与 `app_state()` 相同,
/// Mutex::lock 返回 `MutexGuard` 总是可变的。
pub fn app_state_mut() -> Option<std::sync::MutexGuard<'static, AppState>> {
    app_state()
}

/// 获取 exe 所在目录。
pub fn exe_dir() -> Result<PathBuf, AppError> {
    app_state()
        .map(|app| app.exe_dir.clone())
        .ok_or_else(|| AppError::Msg("应用状态不可用".into()))
}

/// 将字符串转为 null 结尾的 UTF-16 Vec。
pub fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

/// 检查路径存在性, 不存在则返回错误。
pub fn ensure_exists(path: &Path) -> Result<(), AppError> {
    if path.exists() {
        Ok(())
    } else {
        Err(AppError::Msg(format!("文件不存在: {}", path.display())))
    }
}

/// 从多个目录中收集所有 .json 文件, 按文件名排序并去重。
pub fn find_json_configs(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                paths.push(path);
            }
        }
    }

    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    // 按文件名去重而不是按完整路径: 菜单项显示的是文件名, 不同目录下的
    // 同名配置在菜单里根本无法区分。依赖上面已按文件名排序。
    paths.dedup_by(|a, b| a.file_name() == b.file_name());
    paths
}

/// 遍历系统进程快照, 对每个进程调用 `visit(进程名, PID)`。
///
/// `visit` 返回 `false` 时提前结束遍历。
fn for_each_process(mut visit: impl FnMut(&str, u32) -> bool) {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            warn!("CreateToolhelp32Snapshot 失败, 无法枚举进程");
            return;
        };

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if !visit(&name, entry.th32ProcessID) {
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }
}

/// 通过 Win32 ToolHelp API 枚举所有与 `exe_name` 匹配的进程, 返回 PID 列表。
pub fn find_pids_by_name(exe_name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for_each_process(|name, pid| {
        if name.eq_ignore_ascii_case(exe_name) {
            pids.push(pid);
        }
        true
    });
    pids
}

/// 一次进程快照判断 `names` 中每个名字是否有进程在运行, 返回值与入参同序。
///
/// 每个名字单独枚举一遍是纯粹的浪费: CreateToolhelp32Snapshot 要复制整张
/// 进程表, 进程多的机器上单次就要几十毫秒。全部命中后提前结束遍历。
pub fn running_flags<const N: usize>(names: &[&str; N]) -> [bool; N] {
    let mut found = [false; N];
    for_each_process(|name, _| {
        for (flag, target) in found.iter_mut().zip(names) {
            if !*flag && name.eq_ignore_ascii_case(target) {
                *flag = true;
            }
        }
        !found.iter().all(|f| *f)
    });
    found
}

/// 两个核心的安装与运行状态。
pub struct CoreStates {
    pub sing_box: ProcessState,
    pub xray: ProcessState,
}

/// 查询两个核心的安装与运行状态。
///
/// 刻意接收 `exe_dir` 而不是 `&AppState`: 进程枚举和文件存在性检查都是几十
/// 毫秒级的系统调用, 调用方应当先从 Mutex 里复制出 exe_dir、释放锁, 再调用
/// 本函数, 否则打开托盘菜单期间会一直持锁, 阻塞需要写状态的后台线程。
pub fn core_states(exe_dir: &Path) -> CoreStates {
    let [sing_box_installed, xray_installed] = Core::ALL.map(|core| core.exe_path(exe_dir).exists());

    // 两个核心都没装就不必枚举进程
    let [sing_box_running, xray_running] = if sing_box_installed || xray_installed {
        running_flags(&Core::ALL.map(Core::exe_name))
    } else {
        [false; 2]
    };

    CoreStates {
        sing_box: process_state(sing_box_installed, sing_box_running),
        xray: process_state(xray_installed, xray_running),
    }
}

fn process_state(installed: bool, running: bool) -> ProcessState {
    match (installed, running) {
        (false, _) => ProcessState::NotInstalled,
        (true, true) => ProcessState::Running,
        (true, false) => ProcessState::NotRunning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BUSY 是全局状态, 相关断言集中在一个测试里, 避免并行执行时互相干扰。
    #[test]
    fn test_busy_guard_lifecycle() {
        let guard = acquire_busy().expect("空闲时应能获取");
        assert!(acquire_busy().is_none(), "已占用时不应重复获取");
        drop(guard);
        let guard = acquire_busy().expect("释放后应能重新获取");
        drop(guard);

        // panic 展开时同样释放 (dev profile 是 unwind) , 否则一次后台操作
        // panic 就会永久锁死菜单
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(|| {
            let _guard = acquire_busy().expect("panic 测试应能获取");
            panic!("测试用 panic");
        });
        std::panic::set_hook(prev_hook);
        assert!(result.is_err(), "闭包应当 panic");
        assert!(acquire_busy().is_some(), "panic 展开后应已释放");
    }

    /// FlagGuard 对任意静态标志都应满足同样的契约。
    #[test]
    fn test_flag_guard_on_custom_flag() {
        static FLAG: AtomicBool = AtomicBool::new(false);

        let guard = FlagGuard::acquire(&FLAG).expect("空闲时应能获取");
        assert!(FlagGuard::acquire(&FLAG).is_none(), "已占用时不应重复获取");
        drop(guard);
        assert!(FlagGuard::acquire(&FLAG).is_some(), "释放后应能重新获取");
    }

    /// 用测试进程自己做样本, 不依赖环境里存在某个特定进程。
    fn current_exe_name() -> String {
        std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn test_running_flags_matches_by_name() {
        let name = current_exe_name();
        let [found, missing] = running_flags(&[name.as_str(), "绝对不存在的进程-zzz.exe"]);
        assert!(found, "当前进程 {name} 应被检测到");
        assert!(!missing, "不存在的进程名不应被检测到");
    }

    #[test]
    fn test_running_flags_ignores_case() {
        let upper = current_exe_name().to_uppercase();
        let [found] = running_flags(&[upper.as_str()]);
        assert!(found, "进程名匹配应当大小写不敏感");
    }

    #[test]
    fn test_find_pids_by_name_includes_self() {
        let pids = find_pids_by_name(&current_exe_name());
        assert!(pids.contains(&std::process::id()), "应包含当前进程 PID");
    }

    #[test]
    fn test_process_state_mapping() {
        assert!(matches!(process_state(false, false), ProcessState::NotInstalled));
        assert!(matches!(process_state(false, true), ProcessState::NotInstalled));
        assert!(matches!(process_state(true, false), ProcessState::NotRunning));
        assert!(matches!(process_state(true, true), ProcessState::Running));
    }

    /// 这些路径是 Release 目录结构的一部分, 改动会破坏已有安装, 钉在测试里。
    #[test]
    fn test_core_paths() {
        let base = Path::new("base");

        assert_eq!(Core::SingBox.exe_path(base), base.join("sing-box_core/sing-box.exe"));
        assert_eq!(Core::Xray.exe_path(base), base.join("xray_core/xray.exe"));
        assert_eq!(Core::SingBox.active_config(base), base.join("configs/sing-box.json"));
        assert_eq!(Core::Xray.active_config(base), base.join("configs/xray.json"));
        assert_eq!(Core::SingBox.config_dir(base), base.join("configs/sing-box"));
        assert_eq!(Core::Xray.config_dir(base), base.join("configs/xray"));
    }

    /// 可执行文件名在 `exe_name` 和 `artifacts().required` 里各写了一遍, 两处
    /// 不一致时, 版本检查读的路径就和更新替换的文件对不上: 更新会报"缺少必需
    /// 文件", 或者换掉一个没人加载的文件。
    #[test]
    fn test_artifacts_required_contains_exe() {
        for core in Core::ALL {
            assert!(
                core.artifacts().required.contains(&core.exe_name()),
                "{} 的 required 不含 {}",
                core.label(),
                core.exe_name()
            );
        }
    }

    #[test]
    fn test_find_json_configs_sorts_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.json"), "{}").unwrap();
        fs::write(dir.path().join("a.json"), "{}").unwrap();
        fs::write(dir.path().join("c.txt"), "x").unwrap();

        let found = find_json_configs(&[dir.path().to_path_buf()]);

        let names: Vec<_> = found.iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(names, ["a.json", "b.json"], "只收 .json 且按文件名排序");
    }

    /// 不同目录下的同名配置在菜单里显示成同一个名字, 必须去重。
    #[test]
    fn test_find_json_configs_dedups_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("same.json"), "{}").unwrap();
        fs::write(second.join("same.json"), "{}").unwrap();
        fs::write(second.join("other.json"), "{}").unwrap();

        let found = find_json_configs(&[first, second]);

        let names: Vec<_> = found.iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(names, ["other.json", "same.json"]);
    }

    #[test]
    fn test_find_json_configs_skips_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_json_configs(&[dir.path().join("不存在")]);
        assert!(found.is_empty());
    }
}
