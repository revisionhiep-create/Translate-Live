use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::{
    ImageList_Create, ImageList_Destroy, ImageList_ReplaceIcon, HIMAGELIST, ILC_COLOR32, ILC_MASK,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    DefSubclassProc, FileOpenDialog, IFileOpenDialog, IShellItem, ITaskbarList3,
    RemoveWindowSubclass, SHCreateItemFromParsingName, SetWindowSubclass, ShellExecuteW,
    Shell_NotifyIconW, TaskbarList, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_LARGE_ICON, NIIF_NOSOUND, NIIF_USER,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, SIGDN_FILESYSPATH, TBPF_INDETERMINATE,
    TBPF_NOPROGRESS, TBPF_NORMAL, THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_BITMAP,
    THB_FLAGS, THB_TOOLTIP, THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, DestroyIcon, DestroyMenu,
    GetCursorPos, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, ShowWindow,
    TrackPopupMenu, HICON, HMENU, MF_ENABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSGFLT_ALLOW,
    SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_DISPLAYCHANGE, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCDESTROY,
    WM_NULL, WM_RBUTTONUP, WM_USER,
};

use crate::icon;

const SUBCLASS_ID: usize = 0x544C_5442;
const BTN_TOGGLE_ID: u32 = 1001;
const BTN_QUIT_ID: u32 = 1002;
const TRAY_ICON_ID: u32 = 1;

const WM_APP_SYNC_SHELL: u32 = WM_APP + 20;
const WM_APP_SHOW_WINDOW: u32 = WM_APP + 21;
const WM_APP_HIDE_WINDOW: u32 = WM_APP + 22;
const WM_APP_CLOSE_WINDOW: u32 = WM_APP + 23;
const WM_APP_SET_PROGRESS: u32 = WM_APP + 24;
const WM_APP_TRAY_ICON: u32 = WM_APP + 25;
const WM_APP_NOTIFY: u32 = WM_APP + 26;

const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;

const MENU_ID_SHOW: usize = 1;
const MENU_ID_CAPTURE: usize = 2;
const MENU_ID_OVERLAY: usize = 3;
const MENU_ID_QUIT: usize = 4;

#[derive(Debug, Clone, Copy)]
pub enum ShellCommand {
    ToggleCapture,
    ToggleOverlay,
    ShowWindow,
    DisplayChanged,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarProgress {
    None,
    Indeterminate,
    Determinate { current: u64, total: u64 },
}

#[derive(Debug, Clone)]
struct TrayNotification {
    title: String,
    body: String,
    active_icon: bool,
}

fn fill_utf16<const N: usize>(buf: &mut [u16; N], text: &str) {
    buf.fill(0);
    if N <= 1 {
        return;
    }
    for (dst, ch) in buf.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *dst = ch;
    }
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn format_tray_tip(
    is_on: bool,
    is_busy: bool,
    hotkey_label: &str,
    progress: TaskbarProgress,
) -> String {
    match progress {
        TaskbarProgress::Determinate { current, total } if total > 0 => {
            let pct = ((current.saturating_mul(100)) / total).min(100);
            format!("Translate Live — Downloading ({pct}%)")
        }
        TaskbarProgress::Indeterminate if is_busy => "Translate Live — Loading...".to_string(),
        _ if is_busy => "Translate Live — Working...".to_string(),
        _ if is_on && !hotkey_label.is_empty() => format!("Translate Live — Listening ({hotkey_label})"),
        _ if is_on => "Translate Live — Listening".to_string(),
        _ => "Translate Live — Ready".to_string(),
    }
}

struct ThumbState {
    hwnd: HWND,
    msg_taskbar_created: u32,
    msg_shell_taskbar_created: u32,
    tbl: Option<ITaskbarList3>,
    himl: HIMAGELIST,
    buttons_added: bool,
    tray_added: bool,
    tray_hicon_off: HICON,
    tray_hicon_on: HICON,
    balloon_hicon_off: HICON,
    balloon_hicon_on: HICON,
    is_on: bool,
    is_busy: bool,
    overlay_visible: Arc<Mutex<bool>>,
    hotkey_label: Arc<Mutex<String>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    pending_notification: Arc<Mutex<Option<TrayNotification>>>,
    on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
}

impl Drop for ThumbState {
    fn drop(&mut self) {
        unsafe {
            if self.tray_added {
                remove_tray_icon(self.hwnd);
                self.tray_added = false;
            }
            for icon in [
                &mut self.tray_hicon_off,
                &mut self.tray_hicon_on,
                &mut self.balloon_hicon_off,
                &mut self.balloon_hicon_on,
            ] {
                if !icon.is_invalid() {
                    let _ = DestroyIcon(*icon);
                    *icon = HICON::default();
                }
            }
            if !self.himl.is_invalid() {
                let _ = ImageList_Destroy(self.himl);
                self.himl = HIMAGELIST::default();
            }
        }
    }
}

fn build_thumb_buttons(is_on: bool, is_busy: bool) -> [THUMBBUTTON; 2] {
    let mut toggle = THUMBBUTTON::default();
    toggle.dwMask = THB_BITMAP | THB_TOOLTIP | THB_FLAGS;
    toggle.iId = BTN_TOGGLE_ID;
    toggle.iBitmap = if is_on { 1 } else { 0 };
    toggle.dwFlags = if is_busy { THBF_DISABLED } else { THBF_ENABLED };
    let tip_text = if is_busy {
        "Working..."
    } else if is_on {
        "Stop Capture"
    } else {
        "Start Capture"
    };
    fill_utf16(&mut toggle.szTip, tip_text);

    let mut quit = THUMBBUTTON::default();
    quit.dwMask = THB_BITMAP | THB_TOOLTIP | THB_FLAGS;
    quit.iId = BTN_QUIT_ID;
    quit.iBitmap = 2;
    quit.dwFlags = THBF_ENABLED;
    fill_utf16(&mut quit.szTip, "Quit Translate Live");

    [toggle, quit]
}

unsafe fn apply_taskbar_progress(hwnd: HWND, tbl: &ITaskbarList3, progress: TaskbarProgress) {
    match progress {
        TaskbarProgress::None => {
            let _ = tbl.SetProgressState(hwnd, TBPF_NOPROGRESS);
        }
        TaskbarProgress::Indeterminate => {
            let _ = tbl.SetProgressState(hwnd, TBPF_INDETERMINATE);
        }
        TaskbarProgress::Determinate { current, total } => {
            let _ = tbl.SetProgressState(hwnd, TBPF_NORMAL);
            let _ = tbl.SetProgressValue(hwnd, current, total.max(1));
        }
    }
}

unsafe fn ensure_tray_icon(hwnd: HWND, state: &mut ThumbState, force_readd: bool) {
    if state.tray_hicon_off.is_invalid() {
        if let Ok(h) = icon::tray_hicon(false) {
            state.tray_hicon_off = h;
        }
    }
    if state.tray_hicon_on.is_invalid() {
        if let Ok(h) = icon::tray_hicon(true) {
            state.tray_hicon_on = h;
        }
    }

    let label = state.hotkey_label.lock().unwrap().clone();
    let progress = *state.progress.lock().unwrap();
    let tip = format_tray_tip(state.is_on, state.is_busy, &label, progress);

    let hicon = if state.is_on {
        state.tray_hicon_on
    } else {
        state.tray_hicon_off
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY_ICON,
        ..Default::default()
    };
    if !hicon.is_invalid() {
        nid.uFlags |= NIF_ICON;
        nid.hIcon = hicon;
    }
    fill_utf16(&mut nid.szTip, &tip);

    if !state.tray_added || force_readd {
        state.tray_added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    } else if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        state.tray_added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
    }
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_notification(
    hwnd: HWND,
    state: &mut ThumbState,
    notif: &TrayNotification,
) {
    if !state.tray_added {
        ensure_tray_icon(hwnd, state, false);
    }

    let clear_nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_MODIFY, &clear_nid);

    let balloon_hicon = if notif.active_icon {
        if state.balloon_hicon_on.is_invalid() {
            if let Ok(h) = icon::notification_hicon(true) {
                state.balloon_hicon_on = h;
            }
        }
        state.balloon_hicon_on
    } else {
        if state.balloon_hicon_off.is_invalid() {
            if let Ok(h) = icon::notification_hicon(false) {
                state.balloon_hicon_off = h;
            }
        }
        state.balloon_hicon_off
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    fill_utf16(&mut nid.szInfoTitle, &notif.title);
    fill_utf16(&mut nid.szInfo, &notif.body);

    if !balloon_hicon.is_invalid() {
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND;
        nid.hBalloonIcon = balloon_hicon;
    } else {
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    }

    if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
        ensure_tray_icon(hwnd, state, true);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

struct PopupMenuGuard(HMENU);

impl Drop for PopupMenuGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
}

unsafe fn show_tray_context_menu(
    hwnd: HWND,
    is_on: bool,
    is_busy: bool,
    overlay_vis: bool,
    on_cmd: &Arc<dyn Fn(ShellCommand) + Send + Sync>,
) {
    let Ok(hmenu) = CreatePopupMenu() else {
        return;
    };
    let _menu_guard = PopupMenuGuard(hmenu);

    let show_w = wide_null("Show Control Panel");
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_ENABLED,
        MENU_ID_SHOW,
        PCWSTR(show_w.as_ptr()),
    );

    let (cap_label, cap_flags) = if is_busy {
        ("Working...", MF_STRING | MF_GRAYED)
    } else if is_on {
        ("Stop Live Capture", MF_STRING | MF_ENABLED)
    } else {
        ("Start Live Capture", MF_STRING | MF_ENABLED)
    };
    let cap_w = wide_null(cap_label);
    let _ = AppendMenuW(hmenu, cap_flags, MENU_ID_CAPTURE, PCWSTR(cap_w.as_ptr()));

    let ov_label = if overlay_vis {
        "Hide Subtitle Overlay"
    } else {
        "Show Subtitle Overlay"
    };
    let ov_w = wide_null(ov_label);
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_ENABLED,
        MENU_ID_OVERLAY,
        PCWSTR(ov_w.as_ptr()),
    );

    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

    let quit_w = wide_null("Quit");
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_ENABLED,
        MENU_ID_QUIT,
        PCWSTR(quit_w.as_ptr()),
    );

    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }
    let _ = SetForegroundWindow(hwnd);
    let selected = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    match selected.0 as usize {
        MENU_ID_SHOW => on_cmd(ShellCommand::ShowWindow),
        MENU_ID_CAPTURE => on_cmd(ShellCommand::ToggleCapture),
        MENU_ID_OVERLAY => on_cmd(ShellCommand::ToggleOverlay),
        MENU_ID_QUIT => on_cmd(ShellCommand::Quit),
        _ => {}
    }
}

unsafe fn ensure_thumbbar(hwnd: HWND, state: &mut ThumbState, force_readd: bool) {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    if state.himl.is_invalid() {
        let himl = ImageList_Create(24, 24, ILC_COLOR32 | ILC_MASK, 3, 0);
        if !himl.is_invalid() {
            if let Ok(h_off) = icon::thumb_power_hicon(false) {
                let _ = ImageList_ReplaceIcon(himl, -1, h_off);
                let _ = DestroyIcon(h_off);
            }
            if let Ok(h_on) = icon::thumb_power_hicon(true) {
                let _ = ImageList_ReplaceIcon(himl, -1, h_on);
                let _ = DestroyIcon(h_on);
            }
            if let Ok(h_quit) = icon::thumb_quit_hicon() {
                let _ = ImageList_ReplaceIcon(himl, -1, h_quit);
                let _ = DestroyIcon(h_quit);
            }
            state.himl = himl;
        }
    }

    if state.tbl.is_none() || force_readd {
        if let Ok(tbl) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        {
            if tbl.HrInit().is_ok() {
                state.tbl = Some(tbl);
                state.buttons_added = false;
            }
        }
    }

    let Some(tbl) = state.tbl.as_ref() else {
        return;
    };

    let buttons = build_thumb_buttons(state.is_on, state.is_busy);
    if !state.buttons_added {
        if !state.himl.is_invalid() {
            let _ = tbl.ThumbBarSetImageList(hwnd, state.himl);
        }
        if tbl.ThumbBarAddButtons(hwnd, &buttons).is_ok() {
            state.buttons_added = true;
        }
    } else if tbl.ThumbBarUpdateButtons(hwnd, &buttons).is_err() {
        if !state.himl.is_invalid() {
            let _ = tbl.ThumbBarSetImageList(hwnd, state.himl);
        }
        if tbl.ThumbBarAddButtons(hwnd, &buttons).is_ok() {
            state.buttons_added = true;
        }
    }

    let progress = *state.progress.lock().unwrap();
    apply_taskbar_progress(hwnd, tbl, progress);
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    u_msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    u_id_subclass: usize,
    dw_ref_data: usize,
) -> LRESULT {
    if dw_ref_data == 0 {
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    if u_msg == WM_DISPLAYCHANGE {
        let on_cmd = {
            let state = &*(dw_ref_data as *const ThumbState);
            Arc::clone(&state.on_cmd)
        };
        on_cmd(ShellCommand::DisplayChanged);
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    if u_msg == WM_APP_TRAY_ICON {
        let evt = (l_param.0 as u32) & 0xFFFF;
        match evt {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_BALLOONUSERCLICK => {
                let on_cmd = {
                    let state = &*(dw_ref_data as *const ThumbState);
                    Arc::clone(&state.on_cmd)
                };
                on_cmd(ShellCommand::ShowWindow);
                return LRESULT(0);
            }
            WM_RBUTTONUP => {
                let (is_on, is_busy, ov_vis, on_cmd) = {
                    let state = &*(dw_ref_data as *const ThumbState);
                    (
                        state.is_on,
                        state.is_busy,
                        *state.overlay_visible.lock().unwrap(),
                        Arc::clone(&state.on_cmd),
                    )
                };
                show_tray_context_menu(hwnd, is_on, is_busy, ov_vis, &on_cmd);
                return LRESULT(0);
            }
            _ => return LRESULT(0),
        }
    }

    if u_msg == WM_COMMAND {
        let wp = w_param.0 as u32;
        let hi = (wp >> 16) & 0xFFFF;
        let lo = wp & 0xFFFF;
        if hi == THBN_CLICKED {
            let on_cmd = {
                let state = &*(dw_ref_data as *const ThumbState);
                Arc::clone(&state.on_cmd)
            };
            if lo == BTN_TOGGLE_ID {
                on_cmd(ShellCommand::ToggleCapture);
                return LRESULT(0);
            } else if lo == BTN_QUIT_ID {
                on_cmd(ShellCommand::Quit);
                return LRESULT(0);
            }
        }
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    if u_msg == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), u_id_subclass);
        let boxed = Box::from_raw(dw_ref_data as *mut ThumbState);
        drop(boxed);
        return DefSubclassProc(hwnd, u_msg, w_param, l_param);
    }

    let state = &mut *(dw_ref_data as *mut ThumbState);

    if u_msg == state.msg_taskbar_created {
        ensure_thumbbar(hwnd, state, true);
    } else if u_msg == state.msg_shell_taskbar_created {
        ensure_tray_icon(hwnd, state, true);
    } else if u_msg == WM_APP_SYNC_SHELL {
        state.is_on = w_param.0 != 0;
        state.is_busy = l_param.0 != 0;
        ensure_thumbbar(hwnd, state, false);
        ensure_tray_icon(hwnd, state, false);
        return LRESULT(0);
    } else if u_msg == WM_APP_SET_PROGRESS {
        let progress = *state.progress.lock().unwrap();
        if let Some(tbl) = state.tbl.as_ref() {
            apply_taskbar_progress(hwnd, tbl, progress);
        }
        ensure_tray_icon(hwnd, state, false);
        return LRESULT(0);
    } else if u_msg == WM_APP_NOTIFY {
        let notif = state.pending_notification.lock().unwrap().take();
        if let Some(n) = notif {
            show_tray_notification(hwnd, state, &n);
        }
        return LRESULT(0);
    } else if u_msg == WM_APP_SHOW_WINDOW {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
        return LRESULT(0);
    } else if u_msg == WM_APP_HIDE_WINDOW {
        let _ = ShowWindow(hwnd, SW_HIDE);
        return LRESULT(0);
    } else if u_msg == WM_APP_CLOSE_WINDOW {
        let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        return LRESULT(0);
    }

    DefSubclassProc(hwnd, u_msg, w_param, l_param)
}

#[derive(Clone)]
pub struct ShellBridge {
    hwnd_raw: isize,
    hotkey_label: Arc<Mutex<String>>,
    overlay_visible: Arc<Mutex<bool>>,
    progress: Arc<Mutex<TaskbarProgress>>,
    pending_notification: Arc<Mutex<Option<TrayNotification>>>,
}

impl ShellBridge {
    pub fn install(
        hwnd_raw: isize,
        initial_hotkey_label: String,
        initial_overlay_visible: bool,
        on_cmd: Arc<dyn Fn(ShellCommand) + Send + Sync>,
    ) -> Result<Self> {
        let hotkey_label = Arc::new(Mutex::new(initial_hotkey_label));
        let overlay_visible = Arc::new(Mutex::new(initial_overlay_visible));
        let progress = Arc::new(Mutex::new(TaskbarProgress::None));
        let pending_notification = Arc::new(Mutex::new(None));
        let hwnd = HWND(hwnd_raw as *mut _);

        unsafe {
            let msg_taskbar_created =
                RegisterWindowMessageW(windows::core::w!("TaskbarButtonCreated"));
            let msg_shell_taskbar_created =
                RegisterWindowMessageW(windows::core::w!("TaskbarCreated"));
            let _ = ChangeWindowMessageFilterEx(hwnd, msg_taskbar_created, MSGFLT_ALLOW, None);
            let _ =
                ChangeWindowMessageFilterEx(hwnd, msg_shell_taskbar_created, MSGFLT_ALLOW, None);
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_COMMAND, MSGFLT_ALLOW, None);

            let state = Box::new(ThumbState {
                hwnd,
                msg_taskbar_created,
                msg_shell_taskbar_created,
                tbl: None,
                himl: HIMAGELIST::default(),
                buttons_added: false,
                tray_added: false,
                tray_hicon_off: HICON::default(),
                tray_hicon_on: HICON::default(),
                balloon_hicon_off: HICON::default(),
                balloon_hicon_on: HICON::default(),
                is_on: false,
                is_busy: false,
                overlay_visible: Arc::clone(&overlay_visible),
                hotkey_label: Arc::clone(&hotkey_label),
                progress: Arc::clone(&progress),
                pending_notification: Arc::clone(&pending_notification),
                on_cmd,
            });
            let ptr = Box::into_raw(state);
            let ok = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, ptr as usize);
            if !ok.as_bool() {
                drop(Box::from_raw(ptr));
            } else {
                let _ = PostMessageW(hwnd, WM_APP_SYNC_SHELL, WPARAM(0), LPARAM(0));
            }
        }

        Ok(Self {
            hwnd_raw,
            hotkey_label,
            overlay_visible,
            progress,
            pending_notification,
        })
    }

    pub fn set_hotkey_label(&self, label: String) {
        *self.hotkey_label.lock().unwrap() = label;
    }

    pub fn set_overlay_visible(&self, visible: bool) {
        *self.overlay_visible.lock().unwrap() = visible;
    }

    pub fn set_progress(&self, next: TaskbarProgress) {
        {
            let mut slot = self.progress.lock().unwrap();
            if *slot == next {
                return;
            }
            *slot = next;
        }
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_SET_PROGRESS, WPARAM(0), LPARAM(0));
        }
    }

    pub fn sync_state(&self, is_on: bool, is_busy: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(
                hwnd,
                WM_APP_SYNC_SHELL,
                WPARAM(is_on as usize),
                LPARAM(is_busy as isize),
            );
        }
    }

    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>, active_icon: bool) {
        if self.hwnd_raw == 0 {
            return;
        }
        *self.pending_notification.lock().unwrap() = Some(TrayNotification {
            title: title.into(),
            body: body.into(),
            active_icon,
        });
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_NOTIFY, WPARAM(0), LPARAM(0));
        }
    }

    pub fn show_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_SHOW_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn hide_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_HIDE_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn request_close_window(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_CLOSE_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn shutdown(&self) {
        if self.hwnd_raw == 0 {
            return;
        }
        let hwnd = HWND(self.hwnd_raw as *mut _);
        unsafe {
            remove_tray_icon(hwnd);
        }
    }
}

struct ComInitGuard(bool);

impl Drop for ComInitGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Opens a native Windows folder selection dialog. Must be called from an STA thread.
pub fn pick_folder_dialog(initial_dir: Option<&Path>) -> Option<PathBuf> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _com_guard = ComInitGuard(hr.is_ok());

        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = dialog.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM);

        if let Some(dir) = initial_dir {
            if dir.exists() {
                let wide = wide_null(&dir.to_string_lossy());
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
                {
                    let _ = dialog.SetFolder(&item);
                }
            }
        }

        dialog.Show(HWND::default()).ok()?;
        let result_item = dialog.GetResult().ok()?;
        let pwstr = result_item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path_str = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as _));
        path_str.map(PathBuf::from)
    }
}

/// Opens a native Windows image selection dialog for the subtitle overlay background.
pub fn pick_image_file_dialog(initial_file: Option<&Path>) -> Option<PathBuf> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _com_guard = ComInitGuard(hr.is_ok());

        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = dialog.SetOptions(FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM);

        let name_w = wide_null("Images (PNG, JPG, BMP, WebP)");
        let spec_w = wide_null("*.png;*.jpg;*.jpeg;*.bmp;*.webp");
        let filters = [COMDLG_FILTERSPEC {
            pszName: PCWSTR(name_w.as_ptr()),
            pszSpec: PCWSTR(spec_w.as_ptr()),
        }];
        let _ = dialog.SetFileTypes(&filters);

        if let Some(parent) = initial_file.and_then(|p| p.parent()) {
            if parent.exists() {
                let wide = wide_null(&parent.to_string_lossy());
                if let Ok(item) =
                    SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
                {
                    let _ = dialog.SetFolder(&item);
                }
            }
        }

        dialog.Show(HWND::default()).ok()?;
        let result_item = dialog.GetResult().ok()?;
        let pwstr = result_item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path_str = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as _));
        path_str.map(PathBuf::from)
    }
}

pub fn open_folder_in_explorer(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let verb = wide_null("open");
    let target = wide_null(&dir.to_string_lossy());
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOW,
        );
    }
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        dwattribute: u32,
        pvattribute: *const core::ffi::c_void,
        cbattribute: u32,
    ) -> windows::core::HRESULT;
}

/// Syncs the native Windows title bar between light and dark mode.
pub fn set_window_dark_titlebar(hwnd_raw: isize, dark: bool) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    let enabled: i32 = i32::from(dark);
    unsafe {
        let hr = DwmSetWindowAttribute(
            hwnd,
            20, // DWMWA_USE_IMMERSIVE_DARK_MODE
            (&enabled as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
        if hr.is_err() {
            let _ = DwmSetWindowAttribute(
                hwnd,
                19, // Pre-20H1 fallback
                (&enabled as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tray_tip_reflects_state() {
        assert_eq!(
            format_tray_tip(false, false, "F8", TaskbarProgress::None),
            "Translate Live — Ready"
        );
        assert_eq!(
            format_tray_tip(true, false, "F8", TaskbarProgress::None),
            "Translate Live — Listening (F8)"
        );
        assert_eq!(
            format_tray_tip(false, true, "F8", TaskbarProgress::Indeterminate),
            "Translate Live — Loading..."
        );
        assert_eq!(
            format_tray_tip(
                false,
                true,
                "F8",
                TaskbarProgress::Determinate {
                    current: 45,
                    total: 100
                }
            ),
            "Translate Live — Downloading (45%)"
        );
    }
}
