use anyhow::{anyhow, bail, Result};
use std::collections::VecDeque;
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

const WM_APP_PROCESS_QUEUE: u32 = WM_APP + 1;
const WM_APP_QUIT: u32 = WM_APP + 4;

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    ToggleCapture,
    ToggleOverlay,
    ToggleClickThrough,
}

#[derive(Debug, Clone, Copy)]
enum HotkeySlot {
    Capture,
    Overlay,
    ClickThrough,
}

type CmdQueue = Arc<Mutex<VecDeque<(HotkeySlot, Option<HotkeyBinding>, Sender<Result<()>>)>>>;

pub struct HotkeyController {
    thread_id: u32,
    cmd_queue: CmdQueue,
    join: Option<JoinHandle<()>>,
}

impl HotkeyController {
    pub fn spawn(
        recording_hotkey: Arc<AtomicBool>,
        notify: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Self> {
        let (tid_tx, tid_rx) = mpsc::channel();
        let cmd_queue: CmdQueue = Arc::new(Mutex::new(VecDeque::new()));
        let queue_c = Arc::clone(&cmd_queue);

        let join = thread::Builder::new()
            .name("hotkey-loop".to_string())
            .spawn(move || {
                hotkey_thread(tid_tx, queue_c, recording_hotkey, notify);
            })
            .map_err(|e| anyhow!("Failed to spawn hotkey thread: {e}"))?;

        let thread_id = tid_rx
            .recv()
            .map_err(|_| anyhow!("Hotkey thread exited during initialization"))?;

        Ok(Self {
            thread_id,
            cmd_queue,
            join: Some(join),
        })
    }

    fn update_slot(
        &self,
        slot: HotkeySlot,
        label: &str,
        binding: Option<HotkeyBinding>,
    ) -> Result<()> {
        if let Some(b) = binding {
            if !b.is_valid() {
                bail!("Invalid {label} hotkey binding: {}", b.display_label());
            }
        }

        let (tx, rx) = mpsc::channel();
        {
            let mut q = self.cmd_queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back((slot, binding, tx));
        }
        unsafe {
            PostThreadMessageW(self.thread_id, WM_APP_PROCESS_QUEUE, WPARAM(0), LPARAM(0))
                .map_err(|e| anyhow!("Failed to signal {label} hotkey update: {e}"))?;
        }
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("Timed out updating {label} hotkey"))?
    }

    pub fn set_capture_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(HotkeySlot::Capture, "Live Capture", binding)
    }

    pub fn set_overlay_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(HotkeySlot::Overlay, "Subtitle Overlay", binding)
    }

    pub fn set_click_through_hotkey(&self, binding: Option<HotkeyBinding>) -> Result<()> {
        self.update_slot(HotkeySlot::ClickThrough, "Click-Through", binding)
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

unsafe fn drain_hotkey_queue(cmd_queue: &CmdQueue) {
    loop {
        let next = cmd_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some((slot, binding, reply)) = next else {
            break;
        };
        let res = match slot {
            HotkeySlot::Capture => apply_hotkey(HOTKEY_CAPTURE_ID, "Live Capture", binding),
            HotkeySlot::Overlay => apply_hotkey(HOTKEY_OVERLAY_ID, "Subtitle Overlay", binding),
            HotkeySlot::ClickThrough => {
                apply_hotkey(HOTKEY_CLICK_THROUGH_ID, "Click-Through", binding)
            }
        };
        let _ = reply.send(res);
    }
}

fn hotkey_thread(
    tid_tx: Sender<u32>,
    cmd_queue: CmdQueue,
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

        while GetMessageW(&mut msg, HWND::default(), 0, 0).0 > 0 {
            match msg.message {
                WM_APP_PROCESS_QUEUE => {
                    drain_hotkey_queue(&cmd_queue);
                }
                WM_APP_QUIT => {
                    drain_hotkey_queue(&cmd_queue);
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
