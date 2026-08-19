//! 程序入口与核心编排。
//!
//! 负责应用生命周期管理 (启动、运行、退出) 、日志初始化、
//! 托盘菜单命令分发, 以及状态刷新。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod dns;
mod error;
mod process;
mod scheduler;
mod settings;
mod state;
mod toast;
mod tray;
mod update;

use error::AppError;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tracing::{Event, debug, error, info, warn};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::prelude::*;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, SetConsoleMode,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetSystemTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
use windows::core::PCWSTR;

use crate::state::ConfigAction;

/// COM 初始化守卫, Drop 时自动调用 CoUninitialize。
struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self, AppError> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|e| AppError::Msg(format!("初始化 COM 失败: {e}")))?;
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// 单实例锁使用的 Global 命名互斥体名称。
///
/// 使用 Global 命名空间实现整机单实例: 即使同一台机器有多个登录会话
/// (如 RDP) , 也只会运行一个实例。本程序修改系统级网络配置, 跨会话
/// 并发运行会互相冲突。
const SINGLE_INSTANCE_MUTEX: &str = "Global\\Ladder.SingleInstance";

/// 单实例互斥体守卫, 防止软件重复启动。
///
/// 第一个实例创建命名互斥体并持有句柄; 后续实例创建同一互斥体时
/// 收到 ERROR_ALREADY_EXISTS, 据此判定已存在运行中的实例。
/// 句柄在 Drop 时关闭, 进程异常退出时内核也会自动释放互斥体, 无残留锁。
struct SingleInstanceGuard {
    handle: HANDLE,
}

impl SingleInstanceGuard {
    /// 尝试获取单实例锁。
    ///
    /// 返回 `Ok(Some(guard))` 表示本实例获得锁; `Ok(None)` 表示已有
    /// 实例在运行; `Err` 表示互斥体创建失败 (如权限不足) , 调用方
    /// 应降级为允许启动, 避免误伤正常使用。
    fn acquire() -> Result<Option<Self>, AppError> {
        let name = state::wide(SINGLE_INSTANCE_MUTEX);
        unsafe {
            let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))
                .map_err(|e| AppError::Msg(format!("创建单实例互斥体失败: {e}")))?;
            let already_exists = std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS.0 as i32);
            if already_exists {
                let _ = CloseHandle(handle);
                Ok(None)
            } else {
                Ok(Some(Self { handle }))
            }
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 在后台线程中安全执行闭包, 捕获 panic 并通过 Toast 通知用户。
fn spawn_safe<F: FnOnce() + Send + 'static>(name: &str, f: F) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Err(e) = result {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知内部错误".to_string());
        error!("后台线程 [{name}] panic: {msg}");
        toast::show_toast("内部错误", &msg);
    }
}

fn main() {
    let _guard = match SingleInstanceGuard::acquire() {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            tray::show_warn(
                0,
                "请勿重复启动",
                "软件已在运行, 请勿重复启动, 你可以在右下角的托盘中找到它",
            );
            return;
        }
        Err(err) => {
            eprintln!("创建单实例互斥体失败: {err}");
            None
        }
    };

    if let Err(err) = run() {
        tray::show_error(0, "启动失败", &err.to_string());
    }
}

/// 自定义日志格式: `{时间戳}{毫秒}Z [{级别}] {消息}`, 级别带 ANSI 颜色。
struct BracketedLevel;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_BLUE: &str = "\x1b[34m";

fn level_color(level: &tracing::Level) -> &'static str {
    match *level {
        tracing::Level::INFO => ANSI_GREEN,
        tracing::Level::WARN => ANSI_YELLOW,
        tracing::Level::ERROR => ANSI_RED,
        tracing::Level::DEBUG => ANSI_BLUE,
        _ => "",
    }
}

impl<S, N> FormatEvent<S, N> for BracketedLevel
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &Event<'_>) -> std::fmt::Result {
        let level = event.metadata().level();
        let color = level_color(level);
        let reset = if writer.has_ansi_escapes() { ANSI_RESET } else { "" };
        let lc = if writer.has_ansi_escapes() { color } else { "" };
        let ts = format_utc_timestamp();
        write!(&mut writer, "{ts} [{lc}{level}{reset}] ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

fn format_utc_timestamp() -> String {
    unsafe {
        let t = GetSystemTime();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        )
    }
}

/// 初始化日志系统。
///
/// 1. 为 Windows 控制台启用 ANSI 转义码支持 (彩色输出)
/// 2. 创建 console_layer (stderr) 和 file_layer (app.log)
/// 3. 日志级别优先使用 RUST_LOG 环境变量, 否则使用 settings.json 中的配置
fn init_logging(exe_dir: &Path, log_level: &str) -> Result<(), AppError> {
    unsafe {
        if let Ok(handle) = GetStdHandle(STD_ERROR_HANDLE) {
            let mut mode = CONSOLE_MODE::default();
            if GetConsoleMode(handle, &mut mode).is_ok() {
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .compact()
        .event_format(BracketedLevel)
        .with_writer(std::io::stderr);

    let log_path = exe_dir.join("app.log");
    let file = std::fs::File::create(log_path).map_err(|e| AppError::Msg(format!("创建日志文件失败: {e}")))?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .compact()
        .event_format(BracketedLevel)
        .with_writer(file);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(console_layer)
        .with(filter)
        .try_init()
        .map_err(|e| AppError::Msg(format!("初始化日志系统失败: {e}")))?;

    Ok(())
}

/// 应用主入口: 初始化 COM → 创建目录 → 加载配置 → 初始化日志和 Toast →
/// 检测版本 → 创建托盘图标 → 运行消息循环 → 退出时清理 GDI 资源。
fn run() -> Result<(), AppError> {
    let _com = ComGuard::new()?;

    let exe_path = std::env::current_exe()?;
    let exe_dir = exe_path
        .parent()
        .ok_or(AppError::Msg("无法获取 exe 所在目录".into()))?
        .to_path_buf();

    fs::create_dir_all(exe_dir.join("sing-box_core"))?;
    fs::create_dir_all(exe_dir.join("xray_core"))?;
    fs::create_dir_all(exe_dir.join("configs").join("sing-box"))?;
    fs::create_dir_all(exe_dir.join("configs").join("xray"))?;

    let app_settings = settings::Settings::load(&exe_dir);

    init_logging(&exe_dir, &app_settings.log.level)?;
    for w in settings::Settings::take_warnings() {
        warn!("{w}");
    }
    info!("程序启动, exe 所在目录: {}", exe_dir.display());
    process::cleanup_network_registry();
    dns::restore_dns_to_dhcp();
    scheduler::ensure_boot_dns_reset_task(&exe_dir);
    toast::setup(&exe_path).map_err(|e| AppError::Msg(format!("初始化 Toast 通知失败: {e}")))?;

    let icon_green = unsafe { tray::load_icon_bitmap(&exe_dir, "green_circle.ico") };
    let icon_yellow = unsafe { tray::load_icon_bitmap(&exe_dir, "yellow_circle.ico") };
    let icon_red = unsafe { tray::load_icon_bitmap(&exe_dir, "red_circle.ico") };

    state::APP
        .set(Mutex::new(state::AppState {
            exe_dir: exe_dir.clone(),
            icon_green,
            icon_yellow,
            icon_red,
            settings: app_settings,
            child_sing_box: None,
            child_xray: None,
        }))
        .map_err(|_| AppError::Msg("初始化状态失败".into()))?;

    unsafe {
        let h_instance = GetModuleHandleW(None)
            .map_err(|e| AppError::Msg(format!("获取模块句柄失败: {e}")))?
            .0 as isize;
        let hwnd = tray::create_window(h_instance)?;
        tray::add_icon(hwnd, h_instance, &exe_dir)?;
        tray::set_tooltip("ladder");
        tray::run_message_loop();
    }

    if let Some(app) = state::app_state() {
        unsafe {
            if app.icon_green != 0 {
                let _ = DeleteObject(HGDIOBJ(app.icon_green as *mut std::ffi::c_void));
            }
            if app.icon_yellow != 0 {
                let _ = DeleteObject(HGDIOBJ(app.icon_yellow as *mut std::ffi::c_void));
            }
            if app.icon_red != 0 {
                let _ = DeleteObject(HGDIOBJ(app.icon_red as *mut std::ffi::c_void));
            }
        }
    }

    Ok(())
}

/// 分发托盘菜单命令。
///
/// 重启/终止/更新/切换配置操作在独立线程中执行 (避免阻塞 UI 线程) ,
/// 每个线程独立初始化 COM。退出操作终止所有进程并销毁窗口。
fn execute_menu_command(hwnd: isize, id: u16, config_actions: &HashMap<u16, ConfigAction>) {
    if !matches!(id, tray::ID_OPEN_DIR | tray::ID_EXIT) && state::BUSY.swap(true, Ordering::SeqCst) {
        toast::show_toast("操作进行中", "请等待当前操作完成");
        return;
    }
    match id {
        tray::ID_RESTART_SING
        | tray::ID_RESTART_XRAY
        | tray::ID_RESTART_ALL
        | tray::ID_STOP_SING
        | tray::ID_STOP_XRAY
        | tray::ID_STOP_ALL => {
            let exe_dir = match state::exe_dir() {
                Ok(d) => d,
                Err(e) => {
                    error!("获取 exe 目录失败: {e}");
                    tray::show_error(hwnd, "操作失败", &e.to_string());
                    return;
                }
            };
            let _ = std::thread::Builder::new()
                .name("bg-restart-stop".into())
                .spawn(move || {
                    let _com = match ComGuard::new() {
                        Ok(c) => Some(c),
                        Err(e) => {
                            warn!("COM 初始化失败: {e}");
                            None
                        }
                    };
                    spawn_safe("restart-stop", move || {
                        let label = match id {
                            tray::ID_RESTART_SING => "重启 sing-box",
                            tray::ID_RESTART_XRAY => "重启 xray",
                            tray::ID_RESTART_ALL => "重启所有服务",
                            tray::ID_STOP_SING => "终止 sing-box",
                            tray::ID_STOP_XRAY => "终止 xray",
                            tray::ID_STOP_ALL => "终止所有服务",
                            _ => "",
                        };
                        info!("{label}");
                        let result = match id {
                            tray::ID_RESTART_SING => process::restart_sing_box_at(&exe_dir),
                            tray::ID_RESTART_XRAY => process::restart_xray_at(&exe_dir),
                            tray::ID_RESTART_ALL => process::restart_all_at(&exe_dir),
                            tray::ID_STOP_SING => process::stop_processes(&["sing-box.exe"]),
                            tray::ID_STOP_XRAY => process::stop_processes(&["xray.exe"]),
                            tray::ID_STOP_ALL => process::stop_all(),
                            _ => unreachable!(),
                        };
                        if let Err(err) = result {
                            error!("操作失败: {err}");
                            toast::show_toast("操作失败", &err.to_string());
                        }
                    });
                    state::BUSY.store(false, Ordering::SeqCst);
                });
        }
        tray::ID_UPDATE_ALL | tray::ID_UPDATE_SING | tray::ID_UPDATE_XRAY => {
            let exe_dir = match state::exe_dir() {
                Ok(d) => d,
                Err(e) => {
                    error!("获取 exe 目录失败: {e}");
                    tray::show_error(hwnd, "操作失败", &e.to_string());
                    return;
                }
            };
            let (gh_proxy, max_retries, delay_secs) = {
                let app = match state::app_state() {
                    Some(a) => a,
                    None => {
                        error!("应用状态不可用");
                        tray::show_error(hwnd, "操作失败", "应用状态不可用");
                        return;
                    }
                };
                let s = &app.settings;
                (
                    s.download.core.gh_proxy.clone(),
                    s.download.retry.max_retries,
                    s.download.retry.delay_secs,
                )
            };
            if let Err(e) = process::stop_all() {
                warn!("更新前终止进程失败: {e}");
            }
            let _ = std::thread::Builder::new().name("bg-update".into()).spawn(move || {
                let _com = match ComGuard::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("COM 初始化失败: {e}");
                        None
                    }
                };
                spawn_safe("update", move || {
                    let label = match id {
                        tray::ID_UPDATE_ALL => "更新所有核心",
                        tray::ID_UPDATE_SING => "更新 sing-box",
                        tray::ID_UPDATE_XRAY => "更新 xray",
                        _ => "",
                    };
                    info!("{label}");
                    let result = match id {
                        tray::ID_UPDATE_ALL => update::update_cores(&exe_dir, &gh_proxy, max_retries, delay_secs),
                        tray::ID_UPDATE_SING => update::update_sing_box(&exe_dir, &gh_proxy, max_retries, delay_secs),
                        tray::ID_UPDATE_XRAY => update::update_xray(&exe_dir, &gh_proxy, max_retries, delay_secs),
                        _ => unreachable!(),
                    };
                    if let Err(e) = result {
                        error!("更新失败: {e}");
                        toast::show_toast("更新失败", &e.to_string());
                    }
                });
                state::BUSY.store(false, Ordering::SeqCst);
            });
        }
        tray::ID_SWITCH_CORE_XRAY | tray::ID_SWITCH_CORE_SING | tray::ID_SWITCH_CORE_BOTH => {
            let new_mode = match id {
                tray::ID_SWITCH_CORE_XRAY => settings::CoreMode::Xray,
                tray::ID_SWITCH_CORE_SING => settings::CoreMode::SingBox,
                tray::ID_SWITCH_CORE_BOTH => settings::CoreMode::Both,
                _ => unreachable!(),
            };
            if let Err(e) = process::stop_all() {
                warn!("切换核心前终止进程失败: {e}");
            }
            {
                let mut app = match state::app_state_mut() {
                    Some(a) => a,
                    None => {
                        error!("应用状态不可用");
                        return;
                    }
                };
                app.settings.core.mode = new_mode;
                if let Err(e) = app.settings.save(&app.exe_dir) {
                    error!("保存核心模式失败: {e}");
                    tray::show_error(hwnd, "操作失败", &e.to_string());
                    return;
                }
            }
            info!("核心模式已切换为: {new_mode:?}");
            state::BUSY.store(false, Ordering::SeqCst);
        }
        tray::ID_OPEN_DIR => {
            let exe_dir = match state::exe_dir() {
                Ok(d) => d,
                Err(e) => {
                    error!("获取 exe 目录失败: {e}");
                    return;
                }
            };
            info!("打开程序目录: {}", exe_dir.display());
            let _ = Command::new("explorer").arg(&exe_dir).spawn();
        }
        tray::ID_EXIT => {
            info!("退出程序");
            if let Err(e) = process::stop_all() {
                warn!("退出时终止进程失败: {e}");
            }
            unsafe {
                let _ = DestroyWindow(HWND(hwnd as *mut std::ffi::c_void));
            }
        }
        _ => {
            let action = match config_actions.get(&id).cloned() {
                Some(a) => a,
                None => return,
            };
            let _ = std::thread::Builder::new()
                .name("bg-config-switch".into())
                .spawn(move || {
                    let _com = match ComGuard::new() {
                        Ok(c) => Some(c),
                        Err(e) => {
                            warn!("COM 初始化失败: {e}");
                            None
                        }
                    };
                    spawn_safe("config-switch", move || {
                        let exe_dir = match state::exe_dir() {
                            Ok(d) => d,
                            Err(e) => {
                                error!("获取 exe 目录失败: {e}");
                                return;
                            }
                        };
                        info!("切换配置: {}", action.path.display());
                        let result = match action.kind {
                            state::ConfigKind::SingBox => {
                                let dest = exe_dir.join("configs").join("sing-box.json");
                                debug!("复制配置: {} -> {}", action.path.display(), dest.display());
                                if let Err(e) = fs::copy(&action.path, &dest) {
                                    error!("复制配置失败: {e}");
                                    toast::show_toast("操作失败", &e.to_string());
                                    return;
                                }
                                process::restart_sing_box_at(&exe_dir)
                            }
                            state::ConfigKind::Xray => {
                                let dest = exe_dir.join("configs").join("xray.json");
                                debug!("复制配置: {} -> {}", action.path.display(), dest.display());
                                if let Err(e) = fs::copy(&action.path, &dest) {
                                    error!("复制配置失败: {e}");
                                    toast::show_toast("操作失败", &e.to_string());
                                    return;
                                }
                                process::restart_xray_at(&exe_dir)
                            }
                        };
                        if let Err(err) = result {
                            error!("操作失败: {err}");
                            toast::show_toast("操作失败", &err.to_string());
                        }
                    });
                    state::BUSY.store(false, Ordering::SeqCst);
                });
        }
    }
}
