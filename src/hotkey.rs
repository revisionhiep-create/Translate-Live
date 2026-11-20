use anyhow::{anyhow, bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY,
};

use crate::config::HotkeyBinding;

const HOTKEY_CAPTURE_ID: i32 = 1;
const HOTKEY_OVERLAY_ID: i32 = 2;
const HOTKEY_CLICK_THROUGH_ID: i32 = 3;

const WM_APP_SET_CAPTURE: u32 = WM_APP + 1;
const WM_APP_SET_OVERLAY: u32 = WM_APP + 2;
const WM_APP_SET_CLICK_THROUGH: u32 = WM_APP + 3;
const WM_APP_QUIT: u32 = WM_APP + 4;

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    ToggleCapture,
    ToggleOverlay,
    ToggleClickThrough,
}

pub struct HotkeyController {
    thread_id: u32,
    pending_reply: Arc<Mutex<Option<Sender<Result<()>>>>>,
    capture_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    overlay_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    click_through_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    join: Option<JoinHandle<()>>,
}

impl HotkeyController {
    pub fn spawn(
        recording_hotkey: Arc<AtomicBool>,
        notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Self> {
        let (tid_tx, tid_rx) = mpsc::channel();
        let pending_reply: Arc<Mutex<Option<Sender<Result<()>>>>> = Arc::new(Mutex::new(None));
        let capture_binding = Arc::new(Mutex::new(None));
        let overlay_binding = Arc::new(Mutex::new(None));
        let click_through_binding = Arc::new(Mutex::new(None));

        let pending_c = Arc::clone(&pending_reply);
        let cap_c = Arc::clone(&capture_binding);
        let ov_c = Arc::clone(&overlay_binding);
        let ct_c = Arc::clone(&click_through_binding);

        let join = thread::Builder::new()
            .name("hotkey-loop".to_string())
            .spawn(move || {
                hotkey_thread(
                    tid_tx,
                    pending_c,
                    cap_c,
                    ov_c,
                    ct_c,
                    recording_hotkey,
                    notify,
                );
            })
            .map_err(|e| anyhow!("Failed to spawn hotkey thread: {e}"))?;

        let thread_id = tid_rx
            .recv()
            .map_err(|_| anyhow!("Hotkey thread exited during initialization"))?;

        Ok(Self {
            thread_id,
            pending_reply,
            capture_binding,
            overlay_binding,
            click_through_binding,
            join: Some(join),
        })
    }

    fn update_slot(
        &self,
        slot: &Arc<Mutex<Option<HotkeyBinding>>>,
        msg_id: u32,
        label: &str,
        binding: Option<HotkeyBinding>,
    ) -> Result<()> {
        if let Some(b) = binding {
            if !b.is_valid() {
                bail!("Invalid {label} hotkey binding: {}", b.display_label());
            }
        }
        *slot.lock().unwrap() = binding;

        let (tx, rx) = mpsc::channel();
        *self.pending_reply.lock().unwrap() = Some(tx);
        unsafe {
            PostThreadMessageW(self.thread_id, msg_id, WPARAM(0), LPARAM(0))
                .map_err(|e| anyhow!("Failed to signal {label} hotkey update: {e}"))?;
        }
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out updating {label} hotkey"))?
    }

    pub fn set_capture_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(&self.capture_binding, WM_APP_SET_CAPTURE, "Live Capture", binding)
    }

    pub fn set_overlay_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(
            &self.overlay_binding,
            WM_APP_SET_OVERLAY,
            "Subtitle Overlay",
            binding,
        )
    }

    pub fn set_click_through_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(
            &self.click_through_binding,
            WM_APP_SET_CLICK_THROUGH,
            "Click-Through",
            binding,
        )
    }

    pub fn shutdown(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HotkeyController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn reply_pending(pending_reply: &Arc<Mutex<Option<Sender<Result<()>>>>>, res: Result<()>) {
    if let Some(tx) = pending_reply.lock().unwrap().take() {
        let _ = tx.send(res);
    }
}

fn build_modifiers(binding: HotkeyBinding) -> HOT_KEY_MODIFIERS {
    let mut mods = MOD_NOREPEAT;
    if binding.ctrl {
        mods |= MOD_CONTROL;
    }
    if binding.alt {
        mods |= MOD_ALT;
    }
    if binding.shift {
        mods |= MOD_SHIFT;
    }
    if binding.win {
        mods |= MOD_WIN;
    }
    mods
}

unsafe fn apply_hotkey(id: i32, name: &str, binding: Option<HotkeyBinding>) -> Result<()> {
    let _ = UnregisterHotKey(None, id);
    if let Some(b) = binding {
        RegisterHotKey(None, id, build_modifiers(b), b.vk).map_err(|_| {
            anyhow!(
                "{} ({name}) is already registered by another application",
                b.display_label()
            )
        })?;
    }
    Ok(())
}

fn hotkey_thread(
    tid_tx: Sender<u32>,
    pending_reply: Arc<Mutex<Option<Sender<Result<()>>>>>,
    capture_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    overlay_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    click_through_binding: Arc<Mutex<Option<HotkeyBinding>>>,
    recording_hotkey: Arc<AtomicBool>,
    notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
) {
    unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_NOREMOVE);
        let tid = GetCurrentThreadId();
        let _ = tid_tx.send(tid);

        let mut last_cap: Option<Instant> = None;
        let mut last_ov: Option<Instant> = None;
        let mut last_ct: Option<Instant> = None;

        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            match msg.message {
                WM_APP_SET_CAPTURE => {
                    let b = *capture_binding.lock().unwrap();
                    let res = apply_hotkey(HOTKEY_CAPTURE_ID, "Live Capture", b);
                    reply_pending(&pending_reply, res);
                }
                WM_APP_SET_OVERLAY => {
                    let b = *overlay_binding.lock().unwrap();
                    let res = apply_hotkey(HOTKEY_OVERLAY_ID, "Subtitle Overlay", b);
                    reply_pending(&pending_reply, res);
                }
                WM_APP_SET_CLICK_THROUGH => {
                    let b = *click_through_binding.lock().unwrap();
                    let res = apply_hotkey(HOTKEY_CLICK_THROUGH_ID, "Click-Through", b);
                    reply_pending(&pending_reply, res);
                }
                WM_APP_QUIT => {
                    let _ = UnregisterHotKey(None, HOTKEY_CAPTURE_ID);
                    let _ = UnregisterHotKey(None, HOTKEY_OVERLAY_ID);
                    let _ = UnregisterHotKey(None, HOTKEY_CLICK_THROUGH_ID);
                    break;
                }
                WM_HOTKEY => {
                    if recording_hotkey.load(Ordering::Relaxed) {
                        continue;
                    }
                    let id = msg.wParam.0 as i32;
                    let now = Instant::now();
                    let cooldown = Duration::from_millis(250);
                    match id {
                        HOTKEY_CAPTURE_ID => {
                            if last_cap.map_or(true, |p| now.duration_since(p) >= cooldown) {
                                last_cap = Some(now);
                                notify(HotkeyEvent::ToggleCapture);
                            }
                        }
                        HOTKEY_OVERLAY_ID => {
                            if last_ov.map_or(true, |p| now.duration_since(p) >= cooldown) {
                                last_ov = Some(now);
                                notify(HotkeyEvent::ToggleOverlay);
                            }
                        }
                        HOTKEY_CLICK_THROUGH_ID => {
                            if last_ct.map_or(true, |p| now.duration_since(p) >= cooldown) {
                                last_ct = Some(now);
                                notify(HotkeyEvent::ToggleClickThrough);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}
