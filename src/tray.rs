//! Windows 系统托盘 UI。
//!
//! 负责创建隐藏窗口、注册托盘图标、构建右键弹出菜单 (重启/终止/更新/切换配置/退出) ,
//! 处理鼠标消息并将菜单事件分发给 `main::execute_menu_command`。

use std::collections::HashMap;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, GetDC,
    HBITMAP, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu, CreateWindowExW, DI_NORMAL, DefWindowProcW,
    DestroyIcon, DestroyMenu, DispatchMessageW, DrawIconEx, GetCursorPos, GetMenuItemCount, GetMessageW, HICON, HMENU,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, LoadIconW, LoadImageW, MB_ICONERROR, MB_ICONWARNING,
    MB_OK, MENUITEMINFOW, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, MSG, MessageBoxW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetMenuItemInfoW, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_DESTROY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR};

use crate::error::AppError;
use crate::settings::CoreMode;
use crate::state::{self, ConfigAction, ConfigKind, ProcessState};
use tracing::warn;

/// 托盘图标的自定义消息 ID, 当托盘收到鼠标事件时通过此消息通知窗口。
pub const WM_TRAY_ICON: u32 = WM_APP + 1;
const TRAY_UID: u32 = 1;

static TRAY_HWND: OnceLock<isize> = OnceLock::new();

// 菜单 ID 分段规划: 101-199 重启, 201-299 终止, 301-399 更新, 999 退出,
// 1000-1999 sing-box 配置, 2000-2999 xray 配置。
pub const ID_RESTART_SING: u16 = 101;
pub const ID_RESTART_XRAY: u16 = 102;
pub const ID_RESTART_ALL: u16 = 103;
pub const ID_STOP_SING: u16 = 201;
pub const ID_STOP_XRAY: u16 = 202;
pub const ID_STOP_ALL: u16 = 203;
pub const ID_UPDATE_ALL: u16 = 301;
pub const ID_UPDATE_SING: u16 = 302;
pub const ID_UPDATE_XRAY: u16 = 303;
pub const ID_SWITCH_CORE_XRAY: u16 = 401;
pub const ID_SWITCH_CORE_SING: u16 = 402;
pub const ID_SWITCH_CORE_BOTH: u16 = 403;
pub const ID_OPEN_DIR: u16 = 998;
pub const ID_EXIT: u16 = 999;
pub const ID_SING_CONFIG_BASE: u16 = 1000;
pub const ID_XRAY_CONFIG_BASE: u16 = 2000;

/// 创建托盘图标所依附的隐藏窗口。返回窗口句柄。
pub unsafe fn create_window(h_instance: isize) -> Result<isize, AppError> {
    unsafe {
        let h_instance = HINSTANCE(h_instance as _);
        let class_name = state::wide("LadderTrayWindow");

        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: h_instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };

        if RegisterClassW(&wnd_class) == 0 {
            return Err(AppError::Msg("注册托盘窗口类失败".into()));
        }

        let title = state::wide("ladder");
        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(h_instance),
            None,
        );

        let Ok(hwnd) = hwnd else {
            return Err(AppError::Msg("创建托盘窗口失败".into()));
        };

        Ok(hwnd.0 as isize)
    }
}

/// 向系统托盘添加图标并注册鼠标事件回调。
pub unsafe fn add_icon(hwnd: isize, h_instance: isize, exe_dir: &Path) -> Result<(), AppError> {
    unsafe {
        let hwnd = HWND(hwnd as _);
        let h_instance = HINSTANCE(h_instance as _);
        let icon = load_app_icon(Some(h_instance), exe_dir);
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY_ICON,
            hIcon: icon,
            szTip: to_wide_padded("ladder"),
            ..Default::default()
        };

        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            return Err(AppError::Msg("添加系统托盘图标失败".into()));
        }

        let _ = TRAY_HWND.set(hwnd.0 as isize);
        Ok(())
    }
}

/// 运行 Windows 消息循环, 阻塞直到窗口被销毁。
pub unsafe fn run_message_loop() {
    unsafe {
        let mut msg: MSG = Default::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 显示错误消息对话框。
pub fn show_error(hwnd: isize, title: &str, message: &str) {
    let hwnd = if hwnd == 0 { None } else { Some(HWND(hwnd as _)) };
    unsafe {
        let _ = MessageBoxW(
            hwnd,
            &HSTRING::from(message),
            &HSTRING::from(title),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 显示警告消息对话框。
pub fn show_warn(hwnd: isize, title: &str, message: &str) {
    let hwnd = if hwnd == 0 { None } else { Some(HWND(hwnd as _)) };
    unsafe {
        let _ = MessageBoxW(
            hwnd,
            &HSTRING::from(message),
            &HSTRING::from(title),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// 更新托盘图标的悬停提示文本。
pub fn set_tooltip(text: &str) {
    if let Some(&hwnd_val) = TRAY_HWND.get() {
        let hwnd = HWND(hwnd_val as _);
        unsafe {
            let nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: TRAY_UID,
                uFlags: NIF_TIP,
                szTip: to_wide_padded(text),
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// 从 ICO 文件加载图标并转换为 32 位 DIB 位图句柄。
///
/// 流程: LoadImageW 加载 ICO → 创建兼容 DC → 创建 DIB Section →
/// DrawIconEx 绘制到位图 → 清理中间资源 → 返回位图句柄。
/// 菜单项需要位图句柄 (而非图标句柄) 来显示状态图标。
pub(crate) unsafe fn load_icon_bitmap(exe_dir: &Path, icon_name: &str) -> isize {
    unsafe {
        let icon_path = exe_dir.join("icons").join(icon_name);
        if !icon_path.exists() {
            warn!("图标文件不存在: {}", icon_path.display());
            return 0;
        }
        let icon_path_w: Vec<u16> = icon_path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
        let hicon = LoadImageW(
            None,
            PCWSTR(icon_path_w.as_ptr()),
            IMAGE_ICON,
            16,
            16,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        );
        let Ok(hicon) = hicon else {
            warn!("加载图标失败: {}", icon_path.display());
            return 0;
        };
        let hicon = HICON(hicon.0);

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            let _ = DestroyIcon(hicon);
            return 0;
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        if mem_dc.is_invalid() {
            let _ = ReleaseDC(None, screen_dc);
            let _ = DestroyIcon(hicon);
            return 0;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 16,
                biHeight: 16,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pv_bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(screen_dc), &bmi, DIB_RGB_COLORS, &mut pv_bits, None, 0) else {
            let _ = DeleteDC(mem_dc);
            let _ = DestroyIcon(hicon);
            return 0;
        };

        let old_bmp = SelectObject(mem_dc, HGDIOBJ(bmp.0));
        let _ = DrawIconEx(mem_dc, 0, 0, hicon, 16, 16, 0, None, DI_NORMAL);
        SelectObject(mem_dc, old_bmp);
        let _ = ReleaseDC(None, screen_dc);
        let _ = DeleteDC(mem_dc);
        let _ = DestroyIcon(hicon);

        bmp.0 as isize
    }
}

/// 窗口过程: 处理托盘图标鼠标事件和窗口销毁。
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY_ICON => {
                let event = lparam.0 as u32;
                if event == WM_LBUTTONUP || event == WM_RBUTTONUP {
                    let (selected, config_actions) = show_tray_menu(hwnd);
                    if selected != 0 {
                        crate::execute_menu_command(hwnd.0 as isize, selected, &config_actions);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                remove_tray_icon(hwnd);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn load_app_icon(h_instance: Option<HINSTANCE>, exe_dir: &Path) -> HICON {
    unsafe {
        let icon_path = exe_dir.join("icons").join("ladder.ico");
        if icon_path.exists() {
            let icon_path_w: Vec<u16> = icon_path.to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let icon = LoadImageW(
                h_instance,
                PCWSTR(icon_path_w.as_ptr()),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            );
            if let Ok(icon) = icon {
                return HICON(icon.0);
            }
            warn!("加载托盘图标失败, 回退到默认图标: {}", icon_path.display());
        } else {
            warn!("托盘图标不存在, 回退到默认图标: {}", icon_path.display());
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 构建并显示右键弹出菜单, 返回用户选择的菜单项 ID 和配置项映射。
///
/// 菜单根据当前核心模式 (xray / sing-box / both) 动态构建。
/// Mutex 仅在读取状态时持有, TrackPopupMenu 在锁外执行,
/// 避免模态消息循环重入窗口过程时因 Mutex 不可重入导致死锁。
unsafe fn show_tray_menu(hwnd: HWND) -> (u16, HashMap<u16, ConfigAction>) {
    unsafe {
        let (sing_state, xray_state, exe_dir, icon_green, icon_yellow, icon_red, core_mode) = {
            let app = match state::app_state() {
                Some(app) => app,
                None => return (0, HashMap::new()),
            };
            (
                state::sing_box_state(&app),
                state::xray_state(&app),
                app.exe_dir.clone(),
                app.icon_green,
                app.icon_yellow,
                app.icon_red,
                app.settings.core.mode,
            )
        };

        let Ok(menu) = CreatePopupMenu() else {
            return (0, HashMap::new());
        };

        let status_hbmp = |s: ProcessState| match s {
            ProcessState::Running => icon_green,
            ProcessState::NotRunning => icon_yellow,
            ProcessState::NotInstalled => icon_red,
        };
        let status_label = |s: ProcessState, name: &str| match s {
            ProcessState::Running => format!("{name} 正在运行"),
            ProcessState::NotRunning => format!("{name} 未在运行"),
            ProcessState::NotInstalled => format!("{name} 未安装"),
        };

        // ── 状态项: 仅显示已启用核心 ──
        if core_mode.runs_sing_box() {
            append_status_item(menu, &status_label(sing_state, "sing-box"), status_hbmp(sing_state));
        }
        if core_mode.runs_xray() {
            append_status_item(menu, &status_label(xray_state, "xray"), status_hbmp(xray_state));
        }
        append_separator(menu);

        // ── 操作项 ──
        let mut config_actions = HashMap::new();

        if core_mode == CoreMode::Both {
            // 双核模式: 重新启动/终止运行/更新核心 均为子菜单
            let restart_menu = new_submenu();
            let stop_menu = new_submenu();
            let update_menu = new_submenu();

            append_item(restart_menu, ID_RESTART_SING, "重启 sing-box");
            append_item(restart_menu, ID_RESTART_XRAY, "重启 xray");
            append_item(restart_menu, ID_RESTART_ALL, "重启 sing-box 和 xray");

            append_item(stop_menu, ID_STOP_SING, "终止 sing-box");
            append_item(stop_menu, ID_STOP_XRAY, "终止 xray");
            append_item(stop_menu, ID_STOP_ALL, "终止 sing-box 和 xray");

            append_item(update_menu, ID_UPDATE_SING, "更新 sing-box");
            append_item(update_menu, ID_UPDATE_XRAY, "更新 xray");
            append_item(update_menu, ID_UPDATE_ALL, "更新 sing-box 和 xray");

            let sing_menu = new_submenu();
            let xray_menu = new_submenu();
            append_config_items(
                &mut config_actions,
                sing_menu,
                ConfigKind::SingBox,
                ID_SING_CONFIG_BASE,
                &[exe_dir.join("configs").join("sing-box")],
            );
            append_config_items(
                &mut config_actions,
                xray_menu,
                ConfigKind::Xray,
                ID_XRAY_CONFIG_BASE,
                &[exe_dir.join("configs").join("xray")],
            );

            append_submenu(menu, restart_menu, "重新启动");
            append_submenu(menu, stop_menu, "终止运行");
            append_submenu(menu, update_menu, "更新核心");
            append_submenu(menu, sing_menu, "切换 sing-box 配置");
            append_submenu(menu, xray_menu, "切换 xray 配置");
        } else {
            // 单核模式: 操作项为直接可点击的一级菜单
            let (restart_id, stop_id, update_id, config_kind, config_base, config_dir) = if core_mode.runs_xray() {
                (
                    ID_RESTART_XRAY,
                    ID_STOP_XRAY,
                    ID_UPDATE_XRAY,
                    ConfigKind::Xray,
                    ID_XRAY_CONFIG_BASE,
                    exe_dir.join("configs").join("xray"),
                )
            } else {
                (
                    ID_RESTART_SING,
                    ID_STOP_SING,
                    ID_UPDATE_SING,
                    ConfigKind::SingBox,
                    ID_SING_CONFIG_BASE,
                    exe_dir.join("configs").join("sing-box"),
                )
            };

            append_item(menu, restart_id, "重新启动");
            append_item(menu, stop_id, "终止运行");
            append_item(menu, update_id, "更新核心");

            let config_menu = new_submenu();
            append_config_items(
                &mut config_actions,
                config_menu,
                config_kind,
                config_base,
                &[config_dir],
            );
            append_submenu(menu, config_menu, "切换配置");
        }

        // ── 切换核心子菜单 (始终显示)  ──
        append_separator(menu);
        let switch_menu = new_submenu();
        append_item_or_disabled(
            switch_menu,
            ID_SWITCH_CORE_XRAY,
            "仅用 xray",
            core_mode == CoreMode::Xray,
        );
        append_item_or_disabled(
            switch_menu,
            ID_SWITCH_CORE_SING,
            "仅用 sing-box",
            core_mode == CoreMode::SingBox,
        );
        append_item_or_disabled(
            switch_menu,
            ID_SWITCH_CORE_BOTH,
            "全部启用",
            core_mode == CoreMode::Both,
        );
        append_submenu(menu, switch_menu, "切换核心");
        append_item(menu, ID_OPEN_DIR, "打开程序目录");
        append_separator(menu);
        append_item(menu, ID_EXIT, "退出并终止");

        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        let _ = DestroyMenu(menu);

        (selected.0 as u16, config_actions)
    }
}

fn new_submenu() -> HMENU {
    unsafe { CreatePopupMenu().unwrap_or_default() }
}

unsafe fn append_separator(menu: HMENU) {
    unsafe {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    }
}

unsafe fn append_item(menu: HMENU, id: u16, label: &str) {
    unsafe {
        let w = state::wide(label);
        let _ = AppendMenuW(menu, MF_STRING, id as usize, PCWSTR(w.as_ptr()));
    }
}

unsafe fn append_disabled_item(menu: HMENU, label: &str) {
    unsafe {
        let w = state::wide(label);
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(w.as_ptr()));
    }
}

/// 添加菜单项, `disabled` 为 true 时灰掉禁用。
unsafe fn append_item_or_disabled(menu: HMENU, id: u16, label: &str, disabled: bool) {
    unsafe {
        let w = state::wide(label);
        let flags = if disabled { MF_STRING | MF_GRAYED } else { MF_STRING };
        let item_id = if disabled { 0 } else { id as usize };
        let _ = AppendMenuW(menu, flags, item_id, PCWSTR(w.as_ptr()));
    }
}

/// 添加带状态位图的菜单项 (仅显示, 不可点击) 。
/// 先 AppendMenuW 创建文本项, 再 SetMenuItemInfoW 设置位图——
/// 这是 Win32 菜单项附加位图的标准做法。
unsafe fn append_status_item(menu: HMENU, label: &str, hbmp: isize) {
    unsafe {
        let w = state::wide(label);
        let position = GetMenuItemCount(Some(menu)) as u32;
        let _ = AppendMenuW(menu, MF_STRING, 0, PCWSTR(w.as_ptr()));

        if hbmp == 0 {
            return;
        }

        let mii = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: HBITMAP(hbmp as _),
            ..Default::default()
        };
        let _ = SetMenuItemInfoW(menu, position, true, &mii);
    }
}

unsafe fn append_submenu(menu: HMENU, submenu: HMENU, label: &str) {
    unsafe {
        let w = state::wide(label);
        let _ = AppendMenuW(menu, MF_STRING | MF_POPUP, submenu.0 as usize, PCWSTR(w.as_ptr()));
    }
}

/// 扫描配置目录并将每个 .json 文件添加为菜单项, 最多 900 项。
unsafe fn append_config_items(
    map: &mut HashMap<u16, ConfigAction>,
    menu: HMENU,
    kind: ConfigKind,
    base_id: u16,
    dirs: &[PathBuf],
) {
    unsafe {
        let mut added = 0;

        for (id, path) in (base_id..).zip(state::find_json_configs(dirs)) {
            if id >= base_id + 900 {
                break;
            }

            let label = path
                .file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("未命名配置")
                .to_string();
            append_item(menu, id, &label);
            map.insert(id, ConfigAction { kind, path });
            added += 1;
        }

        if added == 0 {
            append_disabled_item(menu, "未找到 .json 配置");
        }
    }
}

/// 将字符串转为 UTF-16 并填充到固定长度缓冲区。
/// `N` 是缓冲区长度 (含 null 终止符) , 超出部分被截断。
/// NOTIFYICONDATAW.szTip 要求 128 个 u16 的固定长度数组。
fn to_wide_padded<const N: usize>(s: &str) -> [u16; N] {
    let mut buf = [0u16; N];
    let wide: Vec<u16> = s.encode_utf16().collect();
    let len = wide.len().min(N - 1);
    buf[..len].copy_from_slice(&wide[..len]);
    buf
}
