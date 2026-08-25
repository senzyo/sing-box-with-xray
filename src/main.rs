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
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tracing::{Event, debug, error, info, warn};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
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

/// 安装 panic 钩子, 在进程终止前恢复被本程序改动的系统状态。
///
/// release 构建用 `panic = "abort"` (原因见 Cargo.toml) , panic 不展开, Drop 和
/// catch_unwind 都不会执行。但 panic 钩子会在 abort 之前被调用, 所以把必须做的
/// 清理放在这里。
///
/// 关键是物理网卡 DNS: xray 的 TUN 模式期间它被劫持到 127.0.0.1, 进程直接消失
/// 的话用户既上不了网、又无从察觉原因, 只能等下次启动或开机任务恢复。
///
/// 钩子里刻意不调用 `process::stop_all`: 那条路径要取 `AppState` 的 Mutex, 而
/// panic 完全可能发生在持锁期间, 再取同一把锁就是自死锁。改用不涉及 Mutex 的
/// `kill_cores_without_state`。同理也不弹 Toast: 那要走 COM, 且失败时会回退到
/// 阻塞式 MessageBox, 在即将 abort 的进程里风险大于收益。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<未命名>");
        error!("线程 [{name}] panic, 终止前恢复网络状态: {info}");
        process::kill_cores_without_state();
        default_hook(info);
    }));
}

/// 把操作放到后台线程执行, 避免阻塞托盘的消息循环。
///
/// `guard` 被 move 进线程, 闭包结束时随之释放忙标志。线程创建失败时闭包连同
/// `guard` 一起在此处 drop, 同样不会泄漏忙标志。
///
/// 线程内独立初始化 COM: Toast 通知需要 COM, 而 COM 的初始化是线程局部的。
///
/// 闭包内的 panic 不在这里兜底 —— release 构建是 abort, 展开根本不会发生,
/// 清理由 `install_panic_hook` 负责。`name` 会成为线程名, panic 日志据此
/// 指出是哪个后台操作出的问题。
fn spawn_bg<F: FnOnce() + Send + 'static>(name: &'static str, guard: state::FlagGuard, f: F) {
    let spawned = std::thread::Builder::new().name(name.to_owned()).spawn(move || {
        let _busy = guard;
        let _com = match ComGuard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("COM 初始化失败: {e}");
                None
            }
        };
        f();
    });

    if let Err(e) = spawned {
        error!("创建后台线程 [{name}] 失败: {e}");
        toast::show_toast("操作失败", "无法创建后台线程, 请稍后重试");
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
        write_utc_timestamp(&mut writer)?;
        write!(&mut writer, " [{lc}{level}{reset}] ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// 把当前 UTC 时间戳直接写进输出, 避免每条日志都分配一个 String。
fn write_utc_timestamp(writer: &mut Writer<'_>) -> std::fmt::Result {
    let t = unsafe { GetSystemTime() };
    write!(
        writer,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// 达到日志上限时写入的最后一条提示。
const LOG_LIMIT_NOTICE: &[u8] =
    "\n[日志已达大小上限, 后续内容不再写入。可在 settings.json 的 log.max_size_mb 调整]\n".as_bytes();

/// 带大小上限的日志文件。
///
/// 默认日志级别是 debug, 而子进程的 stderr 每一行都会被转发成 warn。核心进入
/// 错误重试循环时会持续刷日志, 长期挂机可能写出很大的文件。达到上限后追加一
/// 条提示, 之后静默丢弃。
struct CappedFile {
    file: std::fs::File,
    written: u64,
    /// 上限字节数, 0 表示不限制。
    limit: u64,
    notified: bool,
}

impl Write for CappedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.limit == 0 {
            return self.file.write(buf);
        }
        if self.written >= self.limit {
            if !self.notified {
                self.notified = true;
                let _ = self.file.write_all(LOG_LIMIT_NOTICE);
                let _ = self.file.flush();
            }
            // 报告写入成功: 返回错误只会让 tracing 反复往 stderr 抱怨
            return Ok(buf.len());
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// `CappedFile` 的共享句柄, 供 tracing 的 `MakeWriter` 使用。
#[derive(Clone)]
struct CappedLogWriter(Arc<Mutex<CappedFile>>);

impl<'a> MakeWriter<'a> for CappedLogWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for CappedLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// 初始化日志系统。
///
/// 1. 为 Windows 控制台启用 ANSI 转义码支持 (彩色输出)
/// 2. 创建 console_layer (stderr) 和 file_layer (app.log, 带大小上限)
/// 3. 日志级别优先使用 RUST_LOG 环境变量, 否则使用 settings.json 中的配置
fn init_logging(exe_dir: &Path, log: &settings::Log) -> Result<(), AppError> {
    unsafe {
        if let Ok(handle) = GetStdHandle(STD_ERROR_HANDLE) {
            let mut mode = CONSOLE_MODE::default();
            if GetConsoleMode(handle, &mut mode).is_ok() {
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&log.level));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .compact()
        .event_format(BracketedLevel)
        .with_writer(std::io::stderr);

    let log_path = exe_dir.join("app.log");
    let file = std::fs::File::create(log_path).map_err(|e| AppError::Msg(format!("创建日志文件失败: {e}")))?;
    let writer = CappedLogWriter(Arc::new(Mutex::new(CappedFile {
        file,
        written: 0,
        limit: log.max_size_mb.saturating_mul(1024 * 1024),
        notified: false,
    })));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .compact()
        .event_format(BracketedLevel)
        .with_writer(writer);

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

    for core in state::Core::ALL {
        fs::create_dir_all(core.core_dir(&exe_dir))?;
        fs::create_dir_all(core.config_dir(&exe_dir))?;
    }

    let app_settings = settings::Settings::load(&exe_dir);

    init_logging(&exe_dir, &app_settings.log)?;
    // 尽早安装: 之后的任何 panic 都要走清理路径恢复 DNS
    install_panic_hook();
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

/// 以 exe 目录为唯一参数的操作。
type ExeDirFn = fn(&Path) -> Result<(), AppError>;

/// 重启 / 终止类菜单命令。
struct ServiceCommand {
    label: &'static str,
    run: ExeDirFn,
}

/// 核心更新类菜单命令。
struct UpdateCommand {
    label: &'static str,
    run: fn(&Path, &str, u32, u64) -> Result<(), AppError>,
}

/// 把菜单 ID 映射为重启 / 终止命令, 未命中返回 `None`。
///
/// 标签和执行体绑定在同一张表里, 新增菜单项时不会出现"外层 match 加了 ID
/// 但内层忘了写实现"的错配, 因此不再需要 `unreachable!` 兜底。
fn service_command(id: u16) -> Option<ServiceCommand> {
    let cmd = match id {
        tray::ID_RESTART_SING => ServiceCommand {
            label: "重启 sing-box",
            run: process::restart_sing_box_at,
        },
        tray::ID_RESTART_XRAY => ServiceCommand {
            label: "重启 xray",
            run: process::restart_xray_at,
        },
        tray::ID_RESTART_ALL => ServiceCommand {
            label: "重启所有服务",
            run: process::restart_all_at,
        },
        tray::ID_STOP_SING => ServiceCommand {
            label: "终止 sing-box",
            run: |_| process::stop_processes(&[state::Core::SingBox]),
        },
        tray::ID_STOP_XRAY => ServiceCommand {
            label: "终止 xray",
            run: |_| process::stop_processes(&[state::Core::Xray]),
        },
        tray::ID_STOP_ALL => ServiceCommand {
            label: "终止所有服务",
            run: |_| process::stop_all(),
        },
        _ => return None,
    };
    Some(cmd)
}

/// 把菜单 ID 映射为核心更新命令, 未命中返回 `None`。
fn update_command(id: u16) -> Option<UpdateCommand> {
    let cmd = match id {
        tray::ID_UPDATE_ALL => UpdateCommand {
            label: "更新所有核心",
            run: update::update_cores,
        },
        tray::ID_UPDATE_SING => UpdateCommand {
            label: "更新 sing-box",
            run: update::update_sing_box,
        },
        tray::ID_UPDATE_XRAY => UpdateCommand {
            label: "更新 xray",
            run: update::update_xray,
        },
        _ => return None,
    };
    Some(cmd)
}

/// 把菜单 ID 映射为目标核心模式, 未命中返回 `None`。
fn switch_core_mode(id: u16) -> Option<settings::CoreMode> {
    match id {
        tray::ID_SWITCH_CORE_XRAY => Some(settings::CoreMode::Xray),
        tray::ID_SWITCH_CORE_SING => Some(settings::CoreMode::SingBox),
        tray::ID_SWITCH_CORE_BOTH => Some(settings::CoreMode::Both),
        _ => None,
    }
}

/// 分发托盘菜单命令。
///
/// 耗时操作 (重启 / 终止 / 更新 / 切换配置) 都交给后台线程, 避免阻塞消息
/// 循环。忙标志由 `FlagGuard` 管理: 交给后台线程的分支在线程结束时释放,
/// 其余分支在本函数返回时释放, 提前返回不会漏。
fn execute_menu_command(hwnd: isize, id: u16, config_actions: &HashMap<u16, ConfigAction>) {
    // 打开目录和退出不占用忙标志: 前者只是拉起 explorer, 后者必须随时可用。
    match id {
        tray::ID_OPEN_DIR => return open_exe_dir(),
        tray::ID_EXIT => return exit_app(hwnd),
        _ => {}
    }

    let Some(guard) = state::acquire_busy() else {
        toast::show_toast("操作进行中", "请等待当前操作完成");
        return;
    };

    if let Some(cmd) = service_command(id) {
        run_service_command(hwnd, guard, cmd);
    } else if let Some(cmd) = update_command(id) {
        run_update_command(hwnd, guard, cmd);
    } else if let Some(mode) = switch_core_mode(id) {
        run_switch_core(guard, mode);
    } else if let Some(action) = config_actions.get(&id).cloned() {
        run_config_switch(guard, action);
    } else {
        debug!("忽略未知菜单项: {id}");
    }
}

fn open_exe_dir() {
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

fn exit_app(hwnd: isize) {
    info!("退出程序");
    // 这里刻意同步执行: 一旦 DestroyWindow 触发消息循环退出, 进程很快就会
    // 结束, 放到后台线程会来不及恢复物理网卡 DNS。
    if let Err(e) = process::stop_all() {
        warn!("退出时终止进程失败: {e}");
    }
    unsafe {
        let _ = DestroyWindow(HWND(hwnd as *mut std::ffi::c_void));
    }
}

fn run_service_command(hwnd: isize, guard: state::FlagGuard, cmd: ServiceCommand) {
    let exe_dir = match state::exe_dir() {
        Ok(d) => d,
        Err(e) => {
            error!("获取 exe 目录失败: {e}");
            tray::show_error(hwnd, "操作失败", &e.to_string());
            return;
        }
    };
    spawn_bg("bg-restart-stop", guard, move || {
        info!("{}", cmd.label);
        if let Err(err) = (cmd.run)(&exe_dir) {
            error!("操作失败: {err}");
            toast::show_toast("操作失败", &err.to_string());
        }
    });
}

fn run_update_command(hwnd: isize, guard: state::FlagGuard, cmd: UpdateCommand) {
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
    spawn_bg("bg-update", guard, move || {
        info!("{}", cmd.label);
        // 放在后台线程里终止进程: stop_all 会枚举进程、逐块物理网卡读写注册表
        // 恢复 DNS 并刷新 DNS 缓存, 在 UI 线程执行会卡住托盘菜单。
        if let Err(e) = process::stop_all() {
            warn!("更新前终止进程失败: {e}");
        }
        if let Err(e) = (cmd.run)(&exe_dir, &gh_proxy, max_retries, delay_secs) {
            error!("更新失败: {e}");
            toast::show_toast("更新失败", &e.to_string());
        }
    });
}

fn run_switch_core(guard: state::FlagGuard, new_mode: settings::CoreMode) {
    spawn_bg("bg-switch-core", guard, move || {
        // 与更新核心同理: stop_all 涉及进程枚举和注册表读写, 不能在 UI 线程做。
        if let Err(e) = process::stop_all() {
            warn!("切换核心前终止进程失败: {e}");
        }
        {
            let mut app = match state::app_state_mut() {
                Some(a) => a,
                None => {
                    error!("应用状态不可用");
                    toast::show_toast("操作失败", "应用状态不可用");
                    return;
                }
            };
            app.settings.core.mode = new_mode;
            if let Err(e) = app.settings.save(&app.exe_dir) {
                error!("保存核心模式失败: {e}");
                toast::show_toast("操作失败", &e.to_string());
                return;
            }
        }
        info!("核心模式已切换为: {new_mode:?}");
    });
}

fn run_config_switch(guard: state::FlagGuard, action: ConfigAction) {
    spawn_bg("bg-config-switch", guard, move || {
        let exe_dir = match state::exe_dir() {
            Ok(d) => d,
            Err(e) => {
                error!("获取 exe 目录失败: {e}");
                return;
            }
        };
        let restart: ExeDirFn = match action.core {
            state::Core::SingBox => process::restart_sing_box_at,
            state::Core::Xray => process::restart_xray_at,
        };
        let dest = action.core.active_config(&exe_dir);

        info!("切换配置: {}", action.path.display());
        debug!("复制配置: {} -> {}", action.path.display(), dest.display());
        if let Err(e) = fs::copy(&action.path, &dest) {
            error!("复制配置失败: {e}");
            toast::show_toast("操作失败", &e.to_string());
            return;
        }
        if let Err(err) = restart(&exe_dir) {
            error!("操作失败: {err}");
            toast::show_toast("操作失败", &err.to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capped(path: &Path, limit: u64) -> CappedFile {
        CappedFile {
            file: std::fs::File::create(path).unwrap(),
            written: 0,
            limit,
            notified: false,
        }
    }

    #[test]
    fn test_capped_file_stops_growing_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut writer = capped(&path, 100);

        for _ in 0..10 {
            assert_eq!(writer.write(&[b'x'; 40]).unwrap(), 40, "超限后也应报告写入成功");
        }
        writer.flush().unwrap();
        drop(writer);

        // 前三次写满 120 字节后追加一条提示, 之后不再增长
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(size, 120 + LOG_LIMIT_NOTICE.len() as u64);
    }

    #[test]
    fn test_capped_file_zero_limit_is_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.log");
        let mut writer = capped(&path, 0);

        for _ in 0..10 {
            writer.write_all(&[b'x'; 40]).unwrap();
        }
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 400);
    }
}
