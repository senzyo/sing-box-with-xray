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

#[derive(Clone, Copy)]
pub enum ConfigKind {
    SingBox,
    Xray,
}

#[derive(Clone)]
pub struct ConfigAction {
    pub kind: ConfigKind,
    pub path: PathBuf,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    NotInstalled,
    NotRunning,
    Running,
}

/// 全局应用状态。
pub struct AppState {
    /// 可执行文件所在目录, 所有相对路径以此为基准。
    pub exe_dir: PathBuf,
    /// GDI 位图句柄: 绿色 (运行中) 、黄色 (未运行) 、红色 (未安装) 。
    pub icon_green: isize,
    pub icon_yellow: isize,
    pub icon_red: isize,
    pub settings: settings::Settings,
    /// 子进程句柄, 用于直接 kill。
    pub child_sing_box: Option<Child>,
    pub child_xray: Option<Child>,
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

/// 从多个目录中收集所有 .json 文件, 按文件名排序去重。
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
    paths.dedup();
    paths
}

/// 通过 Win32 ToolHelp API 枚举所有与 `exe_name` 匹配的进程, 返回 PID 列表。
pub fn find_pids_by_name(exe_name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            warn!("CreateToolhelp32Snapshot 失败, 无法枚举进程");
            return pids;
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
                let name_bytes = &entry.szExeFile[..end];
                let name = String::from_utf16_lossy(name_bytes);
                if name.eq_ignore_ascii_case(exe_name) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }
    pids
}

/// 检查指定名称的进程是否正在运行 (找到第一个即返回) 。
pub fn is_process_running(exe_name: &str) -> bool {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
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
                if name.eq_ignore_ascii_case(exe_name) {
                    let _ = CloseHandle(snapshot);
                    return true;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    false
}

/// 查询 sing-box 进程运行状态。
pub fn sing_box_state(app: &AppState) -> ProcessState {
    if !app.exe_dir.join("sing-box_core").join("sing-box.exe").exists() {
        return ProcessState::NotInstalled;
    }
    if is_process_running("sing-box.exe") {
        ProcessState::Running
    } else {
        ProcessState::NotRunning
    }
}

/// 查询 xray 进程运行状态。
pub fn xray_state(app: &AppState) -> ProcessState {
    if !app.exe_dir.join("xray_core").join("xray.exe").exists() {
        return ProcessState::NotInstalled;
    }
    if is_process_running("xray.exe") {
        ProcessState::Running
    } else {
        ProcessState::NotRunning
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
}
