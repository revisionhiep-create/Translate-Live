use chrono::Local;
use eframe::egui;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::asr::{run_asr_benchmark, AsrEvent, AsrWorkerHandle};
use crate::audio::{enumerate_audio_devices, AudioCaptureHandle, AudioDeviceList, DEFAULT_MIC_SENTINEL};
use crate::config::{
    asr_model_dir, default_model_dir, default_subtitle_lines, display_asr_name, display_llm_name,
    effective_model_dir, find_asr_preset_by_url, format_hex_color, load_config, normalize_asr_url,
    normalize_hf_url, parse_hex_color, resolve_existing_llm_path, save_config, silero_vad_path,
    AppConfig, AudioVadConfig, ComputeBackend, HideAnimation, HotkeyBinding, LiveSubtitleStyle,
    OverlayConfig, SilenceMode, SubtitleLineConfig, SubtitleLineSource, TextAlign,
    TranscriptConfig, VadMode, WindowState, ASR_PRESETS, DEFAULT_ASR_URL, DEFAULT_LLM_URL,
    LLM_PRESETS, SOURCE_LANGUAGES, TARGET_LANGUAGES,
};
use crate::downloader::{
    clean_partial_files, delete_cache_entry, download_asr_model, download_llm_model,
    ensure_silero_vad_only, inspect_asr_cache, inspect_llm_cache, scan_cache_entries, CacheEntry,
    CacheStatus, DownloadProgress,
};
use crate::hotkey::{HotkeyController, HotkeyEvent};
use crate::icon;
use crate::llm::{LlmHandle, LoadInfo, TranslateRequest, TranslationCoordinator, TranslationResult};
use crate::monitor::{
    apply_window_rect, default_reset_rect, enumerate_monitors, query_window_state,
    sanitize_window_rect, FallbackPlacement,
};
use crate::overlay::{
    FinalizedLine, OverlayDisplayHold, OverlayEvent, OverlayHandle, OverlayPayload,
};
use crate::taskbar::{
    open_folder_in_explorer, pick_folder_dialog, pick_image_file_dialog, set_window_dark_titlebar,
    ShellBridge, ShellCommand, TaskbarProgress,
};
use crate::telemetry::{TelemetrySampler, TelemetrySnapshot};

const MAIN_DEFAULT_W: i32 = 580;
const MAIN_DEFAULT_H: i32 = 900;
const MAIN_MIN_W: i32 = 500;
const MAIN_MIN_H: i32 = 580;

const OVERLAY_DEFAULT_W: i32 = 820;
const OVERLAY_DEFAULT_H: i32 = 196;
const OVERLAY_MIN_W: i32 = 260;
const OVERLAY_MIN_H: i32 = 72;

#[derive(Clone, Copy)]
struct ThemeColors {
    bg_window: egui::Color32,
    bg_card: egui::Color32,
    bg_input: egui::Color32,
    bg_callout: egui::Color32,
    bg_callout_hover: egui::Color32,
    bg_callout_active: egui::Color32,
    bg_keycap: egui::Color32,
    bg_track: egui::Color32,
    border_subtle: egui::Color32,
    border_med: egui::Color32,
    text_primary: egui::Color32,
    text_muted: egui::Color32,
    accent_fill: egui::Color32,
    btn_sec_bg: egui::Color32,
    dot_ready: egui::Color32,
    dot_busy: egui::Color32,
    dot_off: egui::Color32,
    error_bg: egui::Color32,
    error_border: egui::Color32,
    error_text: egui::Color32,
    selection_bg: egui::Color32,
    power_border_off: egui::Color32,
}

impl ThemeColors {
    const fn light() -> Self {
        Self {
            bg_window: egui::Color32::from_rgb(244, 243, 241),
            bg_card: egui::Color32::from_rgb(255, 255, 255),
            bg_input: egui::Color32::from_rgb(250, 250, 249),
            bg_callout: egui::Color32::from_rgb(248, 247, 245),
            bg_callout_hover: egui::Color32::from_rgb(240, 238, 234),
            bg_callout_active: egui::Color32::from_rgb(232, 229, 223),
            bg_keycap: egui::Color32::from_rgb(242, 240, 236),
            bg_track: egui::Color32::from_rgb(231, 229, 226),
            border_subtle: egui::Color32::from_rgb(229, 226, 220),
            border_med: egui::Color32::from_rgb(214, 211, 205),
            text_primary: egui::Color32::from_rgb(31, 29, 27),
            text_muted: egui::Color32::from_rgb(115, 110, 104),
            accent_fill: egui::Color32::from_rgb(52, 168, 83),
            btn_sec_bg: egui::Color32::from_rgb(236, 234, 230),
            dot_ready: egui::Color32::from_rgb(52, 168, 83),
            dot_busy: egui::Color32::from_rgb(181, 137, 61),
            dot_off: egui::Color32::from_rgb(160, 154, 146),
            error_bg: egui::Color32::from_rgb(250, 241, 240),
            error_border: egui::Color32::from_rgb(224, 192, 188),
            error_text: egui::Color32::from_rgb(156, 60, 52),
            selection_bg: egui::Color32::from_rgb(214, 211, 205),
            power_border_off: egui::Color32::from_rgb(200, 196, 190),
        }
    }

    const fn dark() -> Self {
        Self {
            bg_window: egui::Color32::from_rgb(18, 18, 18),         // #121212
            bg_card: egui::Color32::from_rgb(30, 30, 30),           // #1E1E1E
            bg_input: egui::Color32::from_rgb(24, 24, 24),          // #181818
            bg_callout: egui::Color32::from_rgb(36, 36, 36),        // #242424
            bg_callout_hover: egui::Color32::from_rgb(44, 44, 44),  // #2C2C2C
            bg_callout_active: egui::Color32::from_rgb(54, 54, 54), // #363636
            bg_keycap: egui::Color32::from_rgb(42, 42, 42),         // #2A2A2A
            bg_track: egui::Color32::from_rgb(48, 48, 48),          // #303030
            border_subtle: egui::Color32::from_rgb(44, 44, 44),     // #2C2C2C
            border_med: egui::Color32::from_rgb(68, 68, 68),        // #444444
            text_primary: egui::Color32::from_rgb(242, 242, 242),   // #F2F2F2
            text_muted: egui::Color32::from_rgb(161, 161, 161),     // #A1A1A1
            accent_fill: egui::Color32::from_rgb(52, 168, 83),      // #34A853
            btn_sec_bg: egui::Color32::from_rgb(42, 42, 42),
            dot_ready: egui::Color32::from_rgb(52, 168, 83),        // #34A853
            dot_busy: egui::Color32::from_rgb(214, 165, 75),
            dot_off: egui::Color32::from_rgb(126, 126, 126),
            error_bg: egui::Color32::from_rgb(51, 31, 30),
            error_border: egui::Color32::from_rgb(110, 59, 54),
            error_text: egui::Color32::from_rgb(240, 138, 130),
            selection_bg: egui::Color32::from_rgb(64, 74, 68),
            power_border_off: egui::Color32::from_rgb(138, 138, 138),
        }
    }

    fn for_mode(dark_mode: bool) -> Self {
        if dark_mode {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    ControlModels,
    AudioVad,
    OverlayTranscript,
    Cache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Asr,
    Llm,
}

#[derive(Debug, Clone)]
struct DownloadState {
    kind: ModelKind,
    downloaded: u64,
    total: Option<u64>,
    speed_bps: f64,
    label: String,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq)]
enum CapturePhase {
    Off,
    Loading,
    Listening { backend_desc: String },
    Stopping,
}

impl CapturePhase {
    fn is_on(&self) -> bool {
        matches!(self, CapturePhase::Listening { .. })
    }

    fn is_busy(&self) -> bool {
        matches!(self, CapturePhase::Loading | CapturePhase::Stopping)
    }
}

#[derive(Debug, Clone)]
struct TranscriptEntry {
    id: u64,
    timestamp: String,
    original: String,
    translation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyTarget {
    Capture,
    Overlay,
    ClickThrough,
}

struct SharedState {
    capture_phase: CapturePhase,
    download: Option<DownloadState>,
    benchmarking: bool,
    error: Option<String>,
    notice: String,
    active_asr_name: String,
    active_backend_desc: String,
    asr_ms: Option<u128>,
    tl_ms: Option<u128>,
    tok_per_sec: Option<f32>,
    interim_utterance_id: u64,
    last_finalized_utterance_id: u64,
    interim_original: String,
    interim_translation: String,
    interim_confirmed_bytes: usize,
    transcript: Vec<TranscriptEntry>,
    next_transcript_id: u64,
    total_finalized_count: u64,
    transcript_version: u64,
    picked_folder: Option<PathBuf>,
    picked_bg_image: Option<PathBuf>,
    hotkey_revert: Vec<(HotkeyTarget, Option<HotkeyBinding>)>,
    overlay_geometry_update: Option<WindowState>,
    display_changed: bool,
    force_quit: bool,
    cache_refresh_needed: bool,
    overlay_sync_visible: Option<bool>,
    overlay_sync_click_through: Option<bool>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            capture_phase: CapturePhase::Off,
            download: None,
            benchmarking: false,
            error: None,
            notice: "Ready · Select or download models to start".to_string(),
            active_asr_name: "—".to_string(),
            active_backend_desc: "Vulkan GPU".to_string(),
            asr_ms: None,
            tl_ms: None,
            tok_per_sec: None,
            interim_utterance_id: 0,
            last_finalized_utterance_id: 0,
            interim_original: String::new(),
            interim_translation: String::new(),
            interim_confirmed_bytes: 0,
            transcript: Vec::new(),
            next_transcript_id: 1,
            total_finalized_count: 0,
            transcript_version: 0,
            picked_folder: None,
            picked_bg_image: None,
            hotkey_revert: Vec::new(),
            overlay_geometry_update: None,
            display_changed: false,
            force_quit: false,
            cache_refresh_needed: true,
            overlay_sync_visible: None,
            overlay_sync_click_through: None,
        }
    }
}

enum CtrlCmd {
    ToggleCapture,
    StartCapture {
        models_dir: PathBuf,
        asr_url: String,
        llm_url: String,
        audio_vad: AudioVadConfig,
    },
    StopCapture,
    DownloadModel {
        kind: ModelKind,
        url: String,
        models_dir: PathBuf,
    },
    CancelDownload,
    RunBenchmark {
        models_dir: PathBuf,
        asr_url: String,
        llm_url: String,
        audio_vad: AudioVadConfig,
    },
    UpdateAudioVad(AudioVadConfig),
    UpdateHotkey(HotkeyTarget, Option<HotkeyBinding>),
    HotkeyEvent(HotkeyEvent),
    ToggleOverlay,
    ToggleClickThrough,
    RefreshOverlay,
    ClearTranscript,
    AudioFatalError {
        session_id: u64,
        error: String,
    },
    AsrEvent {
        session_id: u64,
        event: AsrEvent,
    },
    TranslationPartial {
        session_id: u64,
        utterance_id: u64,
        original: String,
        translation: String,
        confirmed_bytes: usize,
    },
    TranslationEvent {
        session_id: u64,
        result: anyhow::Result<TranslationResult>,
    },
    ShowWindow,
    Quit,
    ShutdownController,
}

fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if (bytes as f64) >= GB * 0.95 {
        format!("{:.2} GB", (bytes as f64) / GB)
    } else if (bytes as f64) >= MB * 0.1 {
        format!("{:.1} MB", (bytes as f64) / MB)
    } else {
        format!("{:.0} KB", (bytes as f64) / 1024.0)
    }
}

fn format_speed(bps: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bps <= 1.0 {
        "— MB/s".to_string()
    } else {
        format!("{:.1} MB/s", bps / MB)
    }
}

fn format_eta(downloaded: u64, total: Option<u64>, speed_bps: f64) -> Option<String> {
    let total = total?;
    if total <= downloaded || speed_bps <= 1024.0 {
        return None;
    }
    let secs = ((total - downloaded) as f64 / speed_bps).ceil() as u64;
    if secs < 60 {
        Some(format!("~{secs}s left"))
    } else {
        let mins = secs / 60;
        let rem = secs % 60;
        Some(format!("~{mins}m {rem:02}s left"))
    }
}

fn shorten_middle(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars || max_chars < 12 {
        return text.to_string();
    }
    let keep = (max_chars - 3) / 2;
    let start: String = text.chars().take(keep).collect();
    let end: String = text.chars().skip(count - keep).collect();
    format!("{start}...{end}")
}

fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    use egui::Key;
    let vk = match key {
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::Space => 0x20,
        Key::Tab => 0x09,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::ArrowLeft => 0x25,
        Key::ArrowUp => 0x26,
        Key::ArrowRight => 0x27,
        Key::ArrowDown => 0x28,
        Key::Minus => 0xBD,
        Key::Equals => 0xBB,
        Key::OpenBracket => 0xDB,
        Key::CloseBracket => 0xDD,
        Key::Semicolon => 0xBA,
        Key::Quote => 0xDE,
        Key::Comma => 0xBC,
        Key::Period => 0xBE,
        Key::Slash => 0xBF,
        Key::Backtick => 0xC0,
        Key::Backslash => 0xDC,
        _ => return None,
    };
    Some(vk)
}

fn configure_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        ("segoe", r"C:\Windows\Fonts\segoeui.ttf"),
        ("yugothic", r"C:\Windows\Fonts\YuGothM.ttc"),
        ("meiryo", r"C:\Windows\Fonts\meiryo.ttc"),
        ("malgun", r"C:\Windows\Fonts\malgun.ttf"),
        ("msyh", r"C:\Windows\Fonts\msyh.ttc"),
    ];
    let mut loaded_names = Vec::new();
    for (name, path) in candidates {
        if let Ok(bytes) = fs::read(path) {
            fonts
                .font_data
                .insert(name.to_string(), Arc::new(egui::FontData::from_owned(bytes)));
            loaded_names.push(name.to_string());
        }
    }
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for name in loaded_names.iter().rev() {
            prop.insert(0, name.clone());
        }
    }
    if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        for name in &loaded_names {
            if name != "segoe" {
                mono.push(name.clone());
            }
        }
    }
    ctx.set_fonts(fonts);
}

pub struct TranslateLiveApp {
    config: AppConfig,
    asr_url_input: String,
    llm_url_input: String,
    asr_custom_mode: bool,
    llm_custom_mode: bool,
    last_custom_asr_url: String,
    last_custom_llm_url: String,
    focus_asr_url_next: bool,
    focus_llm_url_next: bool,
    dir_input: String,
    active_tab: ActiveTab,
    audio_devices: AudioDeviceList,
    telemetry: TelemetrySampler,
    telemetry_snap: TelemetrySnapshot,
    rms_bits: Arc<AtomicU32>,
    vad_prob_bits: Arc<AtomicU32>,
    shared_models_dir: Arc<Mutex<PathBuf>>,
    shared_asr_url: Arc<Mutex<String>>,
    shared_llm_url: Arc<Mutex<String>>,
    shared_audio_vad: Arc<Mutex<AudioVadConfig>>,
    shared_overlay_cfg: Arc<Mutex<OverlayConfig>>,
    shared_show_notifications: Arc<AtomicBool>,
    shared_recording: Arc<AtomicBool>,
    state: Arc<Mutex<SharedState>>,
    ctrl_tx: Sender<CtrlCmd>,
    ctrl_join: Option<JoinHandle<()>>,
    overlay: Option<Arc<OverlayHandle>>,
    shell: Option<ShellBridge>,
    hwnd_raw: isize,
    last_saved_main: Option<WindowState>,
    last_saved_overlay: Option<WindowState>,
    window_dirty_since: Option<Instant>,
    icon_tex_off: egui::TextureHandle,
    icon_tex_on: egui::TextureHandle,
    last_viewport_active: bool,
    applied_dark_mode: bool,
    recording_hotkey: Option<HotkeyTarget>,
    hotkey_hint: Option<String>,
    browsing_folder: Arc<AtomicBool>,
    browsing_image: Arc<AtomicBool>,
    editing_line_id: Option<u64>,
    cache_entries: Vec<CacheEntry>,
    ui_transcript: Vec<TranscriptEntry>,
    ui_transcript_version: u64,
    ui_total_finalized: u64,
    last_finalized_seen: u64,
    cached_status_key: Option<(PathBuf, String, String)>,
    cached_asr_status: CacheStatus,
    cached_llm_status: CacheStatus,
}

impl TranslateLiveApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut config = load_config();
        let dark_mode = config.dark_mode;
        configure_cjk_fonts(&cc.egui_ctx);
        apply_theme_visuals(&cc.egui_ctx, dark_mode);

        let asr_url_input = config.asr_model_url.clone();
        let llm_url_input = config.llm_model_url.clone();
        let asr_custom_mode = find_asr_preset_by_url(&asr_url_input).is_none();
        let llm_custom_mode = !LLM_PRESETS
            .iter()
            .any(|p| p.url.eq_ignore_ascii_case(llm_url_input.trim()));
        let last_custom_asr_url = if asr_custom_mode {
            asr_url_input.clone()
        } else {
            String::new()
        };
        let last_custom_llm_url = if llm_custom_mode {
            llm_url_input.clone()
        } else {
            String::new()
        };
        let effective_dir = effective_model_dir(&config);
        let dir_input = effective_dir.to_string_lossy().into_owned();

        let shared_models_dir = Arc::new(Mutex::new(effective_dir));
        let shared_asr_url = Arc::new(Mutex::new(asr_url_input.clone()));
        let shared_llm_url = Arc::new(Mutex::new(llm_url_input.clone()));
        let shared_audio_vad = Arc::new(Mutex::new(config.audio_vad.clone()));
        let shared_overlay_cfg = Arc::new(Mutex::new(config.overlay.clone()));
        let shared_show_notifications = Arc::new(AtomicBool::new(config.show_notifications));
        let shared_recording = Arc::new(AtomicBool::new(false));
        let rms_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let vad_prob_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let state = Arc::new(Mutex::new(SharedState::default()));

        let (hw, hh, hbytes_off) = icon::header_icon_rgba(false);
        let tex_off = cc.egui_ctx.load_texture(
            "tl_emblem_off",
            egui::ColorImage::from_rgba_unmultiplied([hw as usize, hh as usize], hbytes_off),
            egui::TextureOptions::LINEAR,
        );
        let (hw_on, hh_on, hbytes_on) = icon::header_icon_rgba(true);
        let tex_on = cc.egui_ctx.load_texture(
            "tl_emblem_on",
            egui::ColorImage::from_rgba_unmultiplied([hw_on as usize, hh_on as usize], hbytes_on),
            egui::TextureOptions::LINEAR,
        );

        let hwnd_raw = cc
            .window_handle()
            .ok()
            .and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
                _ => None,
            })
            .unwrap_or(0);

        if hwnd_raw != 0 {
            set_window_dark_titlebar(hwnd_raw, dark_mode);
            let monitors = enumerate_monitors();
            let resolved = sanitize_window_rect(
                config.main_window,
                MAIN_DEFAULT_W,
                MAIN_DEFAULT_H,
                MAIN_MIN_W,
                MAIN_MIN_H,
                &monitors,
                FallbackPlacement::CenterPrimary,
            );
            apply_window_rect(hwnd_raw, resolved);
            config.main_window = Some(WindowState {
                x: resolved.x,
                y: resolved.y,
                width: resolved.width as f32,
                height: resolved.height as f32,
                maximized: resolved.maximized,
            });
        }

        let (ctrl_tx, ctrl_rx) = mpsc::channel();

        let state_ov = Arc::clone(&state);
        let ctx_ov = cc.egui_ctx.clone();
        let overlay = OverlayHandle::spawn(
            config.overlay.clone(),
            config.overlay_window,
            Arc::new(move |ev| {
                let mut s = state_ov.lock().unwrap();
                match ev {
                    OverlayEvent::GeometryChanged(ws) => s.overlay_geometry_update = Some(ws),
                    OverlayEvent::DisplayChanged => s.display_changed = true,
                }
                ctx_ov.request_repaint();
            }),
        )
        .ok()
        .map(Arc::new);

        if let Some(ws) = overlay.as_ref().and_then(|o| o.current_window_state()) {
            config.overlay_window = Some(ws);
        }

        let shell = if hwnd_raw != 0 {
            let tx_shell = ctrl_tx.clone();
            let state_shell = Arc::clone(&state);
            let ctx_shell = cc.egui_ctx.clone();
            let cap_label = config
                .capture_hotkey
                .map(|b| b.display_label())
                .unwrap_or_default();
            ShellBridge::install(
                hwnd_raw,
                cap_label,
                config.overlay.visible,
                Arc::new(move |cmd| {
                    match cmd {
                        ShellCommand::ToggleCapture => {
                            let _ = tx_shell.send(CtrlCmd::ToggleCapture);
                        }
                        ShellCommand::ToggleOverlay => {
                            let _ = tx_shell.send(CtrlCmd::ToggleOverlay);
                        }
                        ShellCommand::ShowWindow => {
                            let _ = tx_shell.send(CtrlCmd::ShowWindow);
                        }
                        ShellCommand::DisplayChanged => {
                            state_shell.lock().unwrap().display_changed = true;
                        }
                        ShellCommand::Quit => {
                            let _ = tx_shell.send(CtrlCmd::Quit);
                        }
                    }
                    ctx_shell.request_repaint();
                }),
            )
            .ok()
        } else {
            None
        };

        let tx_hk = ctrl_tx.clone();
        let ctx_hk = cc.egui_ctx.clone();
        let hotkey = HotkeyController::spawn(
            Arc::clone(&shared_recording),
            Arc::new(move |evt| {
                let _ = tx_hk.send(CtrlCmd::HotkeyEvent(evt));
                ctx_hk.request_repaint();
            }),
        )
        .expect("Failed to start global hotkey controller");

        let llm = Arc::new(LlmHandle::spawn());

        let audio_devices = enumerate_audio_devices().unwrap_or_default();
        let telemetry = TelemetrySampler::new();
        let telemetry_snap = TelemetrySnapshot {
            gpu_name: telemetry.gpu_name().to_string(),
            ..Default::default()
        };

        let tx_c = ctrl_tx.clone();
        let state_c = Arc::clone(&state);
        let dir_c = Arc::clone(&shared_models_dir);
        let asr_c = Arc::clone(&shared_asr_url);
        let llm_c = Arc::clone(&shared_llm_url);
        let av_c = Arc::clone(&shared_audio_vad);
        let overlay_cfg_c = Arc::clone(&shared_overlay_cfg);
        let notif_c = Arc::clone(&shared_show_notifications);
        let rms_c = Arc::clone(&rms_bits);
        let vad_c = Arc::clone(&vad_prob_bits);
        let shell_c = shell.clone();
        let overlay_c = overlay.clone();
        let ctx_c = cc.egui_ctx.clone();
        let init_cap_hk = config.capture_hotkey;
        let init_ov_hk = config.overlay_hotkey;
        let init_ct_hk = config.click_through_hotkey;

        let ctrl_join = thread::Builder::new()
            .name("app-controller".to_string())
            .spawn(move || {
                controller_loop(
                    ctrl_rx,
                    tx_c,
                    state_c,
                    dir_c,
                    asr_c,
                    llm_c,
                    av_c,
                    overlay_cfg_c,
                    notif_c,
                    rms_c,
                    vad_c,
                    llm,
                    hotkey,
                    shell_c,
                    overlay_c,
                    ctx_c,
                    init_cap_hk,
                    init_ov_hk,
                    init_ct_hk,
                );
            })
            .expect("Failed to spawn controller thread");

        let last_saved_main = config.main_window;
        let last_saved_overlay = config.overlay_window;

        let mut app = Self {
            config,
            asr_url_input,
            llm_url_input,
            asr_custom_mode,
            llm_custom_mode,
            last_custom_asr_url,
            last_custom_llm_url,
            focus_asr_url_next: false,
            focus_llm_url_next: false,
            dir_input,
            active_tab: ActiveTab::ControlModels,
            audio_devices,
            telemetry,
            telemetry_snap,
            rms_bits,
            vad_prob_bits,
            shared_models_dir,
            shared_asr_url,
            shared_llm_url,
            shared_audio_vad,
            shared_overlay_cfg,
            shared_show_notifications,
            shared_recording,
            state,
            ctrl_tx,
            ctrl_join: Some(ctrl_join),
            overlay,
            shell,
            hwnd_raw,
            last_saved_main,
            last_saved_overlay,
            window_dirty_since: None,
            icon_tex_off: tex_off,
            icon_tex_on: tex_on,
            last_viewport_active: false,
            applied_dark_mode: dark_mode,
            recording_hotkey: None,
            hotkey_hint: None,
            browsing_folder: Arc::new(AtomicBool::new(false)),
            browsing_image: Arc::new(AtomicBool::new(false)),
            editing_line_id: None,
            cache_entries: Vec::new(),
            ui_transcript: Vec::new(),
            ui_transcript_version: 0,
            ui_total_finalized: 0,
            last_finalized_seen: 0,
            cached_status_key: None,
            cached_asr_status: CacheStatus::NotDownloaded,
            cached_llm_status: CacheStatus::NotDownloaded,
        };
        app.refresh_cache_entries();
        app
    }

    fn current_models_dir(&self) -> PathBuf {
        let trimmed = self.dir_input.trim();
        if trimmed.is_empty() {
            default_model_dir()
        } else {
            PathBuf::from(trimmed)
        }
    }

    fn save_current_config(&self) {
        let _ = save_config(&self.config);
    }

    fn refresh_cache_entries(&mut self) {
        let dir = self.current_models_dir();
        self.cache_entries = scan_cache_entries(&dir);
        self.cached_status_key = None;
    }

    fn apply_models_dir(&mut self, dir: PathBuf) {
        let def = default_model_dir();
        self.config.model_dir = if dir == def {
            None
        } else {
            Some(dir.to_string_lossy().into_owned())
        };
        self.dir_input = dir.to_string_lossy().into_owned();
        *self.shared_models_dir.lock().unwrap() = dir;
        self.save_current_config();
        self.refresh_cache_entries();
    }

    fn sync_overlay_config(&self) {
        *self.shared_overlay_cfg.lock().unwrap() = self.config.overlay.clone();
        if let Some(ov) = self.overlay.as_ref() {
            ov.set_config(self.config.overlay.clone());
        }
        if let Some(sh) = self.shell.as_ref() {
            sh.set_overlay_visible(self.config.overlay.visible);
        }
        let _ = self.ctrl_tx.send(CtrlCmd::RefreshOverlay);
    }

    fn track_window_geometries(&mut self) {
        let mut changed = false;
        if let Some(ws) = query_window_state(self.hwnd_raw) {
            if self.config.main_window != Some(ws) {
                self.config.main_window = Some(ws);
                changed = true;
            }
        }
        if let Some(ov_ws) = self.state.lock().unwrap().overlay_geometry_update.take() {
            if self.config.overlay_window != Some(ov_ws) {
                self.config.overlay_window = Some(ov_ws);
                changed = true;
            }
        }
        if changed {
            self.window_dirty_since = Some(Instant::now());
        } else if let Some(since) = self.window_dirty_since {
            if since.elapsed() >= Duration::from_millis(400) {
                if self.config.main_window != self.last_saved_main
                    || self.config.overlay_window != self.last_saved_overlay
                {
                    self.last_saved_main = self.config.main_window;
                    self.last_saved_overlay = self.config.overlay_window;
                    self.save_current_config();
                }
                self.window_dirty_since = None;
            }
        }
    }

    fn recover_monitors_if_needed(&mut self) {
        let monitors = enumerate_monitors();
        if self.hwnd_raw != 0 {
            let cur = query_window_state(self.hwnd_raw).or(self.config.main_window);
            let resolved = sanitize_window_rect(
                cur,
                MAIN_DEFAULT_W,
                MAIN_DEFAULT_H,
                MAIN_MIN_W,
                MAIN_MIN_H,
                &monitors,
                FallbackPlacement::CenterPrimary,
            );
            apply_window_rect(self.hwnd_raw, resolved);
            self.config.main_window = Some(WindowState {
                x: resolved.x,
                y: resolved.y,
                width: resolved.width as f32,
                height: resolved.height as f32,
                maximized: resolved.maximized,
            });
        }
        if let Some(ov) = self.overlay.as_ref() {
            let cur_ov = ov.current_window_state().or(self.config.overlay_window);
            let resolved_ov = sanitize_window_rect(
                cur_ov,
                OVERLAY_DEFAULT_W,
                OVERLAY_DEFAULT_H,
                OVERLAY_MIN_W,
                OVERLAY_MIN_H,
                &monitors,
                FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
            );
            ov.apply_rect(resolved_ov);
            self.config.overlay_window = Some(WindowState {
                x: resolved_ov.x,
                y: resolved_ov.y,
                width: resolved_ov.width as f32,
                height: resolved_ov.height as f32,
                maximized: false,
            });
        }
        self.last_saved_main = self.config.main_window;
        self.last_saved_overlay = self.config.overlay_window;
        self.window_dirty_since = None;
        self.save_current_config();
    }

    fn reset_window_positions(&mut self) {
        let monitors = enumerate_monitors();
        let main_rect = default_reset_rect(
            MAIN_DEFAULT_W,
            MAIN_DEFAULT_H,
            MAIN_MIN_W,
            MAIN_MIN_H,
            &monitors,
            FallbackPlacement::CenterPrimary,
        );
        if self.hwnd_raw != 0 {
            apply_window_rect(self.hwnd_raw, main_rect);
        }
        self.config.main_window = Some(WindowState {
            x: main_rect.x,
            y: main_rect.y,
            width: main_rect.width as f32,
            height: main_rect.height as f32,
            maximized: false,
        });

        let ov_rect = default_reset_rect(
            OVERLAY_DEFAULT_W,
            OVERLAY_DEFAULT_H,
            OVERLAY_MIN_W,
            OVERLAY_MIN_H,
            &monitors,
            FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
        );
        if let Some(ov) = self.overlay.as_ref() {
            ov.apply_rect(ov_rect);
        }
        self.config.overlay_window = Some(WindowState {
            x: ov_rect.x,
            y: ov_rect.y,
            width: ov_rect.width as f32,
            height: ov_rect.height as f32,
            maximized: false,
        });
        self.last_saved_main = self.config.main_window;
        self.last_saved_overlay = self.config.overlay_window;
        self.window_dirty_since = None;
        self.save_current_config();
        self.state.lock().unwrap().notice =
            "Window positions reset to primary monitor".to_string();
    }

    fn toggle_recording(&mut self, target: HotkeyTarget) {
        if self.recording_hotkey == Some(target) {
            self.recording_hotkey = None;
            self.hotkey_hint = None;
        } else {
            self.recording_hotkey = Some(target);
            self.hotkey_hint = Some("Press Ctrl/Alt/Shift + key, or F1–F12 (Esc to cancel)".to_string());
        }
        self.shared_recording
            .store(self.recording_hotkey.is_some(), Ordering::Relaxed);
    }

    fn find_hotkey_conflict(
        &self,
        target: HotkeyTarget,
        candidate: HotkeyBinding,
    ) -> Option<&'static str> {
        let slots = [
            (HotkeyTarget::Capture, "Live Capture", self.config.capture_hotkey),
            (HotkeyTarget::Overlay, "Subtitle Overlay", self.config.overlay_hotkey),
            (
                HotkeyTarget::ClickThrough,
                "Click-Through",
                self.config.click_through_hotkey,
            ),
        ];
        slots
            .into_iter()
            .find(|(t, _, b)| *t != target && *b == Some(candidate))
            .map(|(_, name, _)| name)
    }

    fn handle_hotkey_recording(&mut self, ctx: &egui::Context) {
        let Some(target) = self.recording_hotkey else {
            return;
        };
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } = ev
            {
                if key == egui::Key::Escape {
                    self.recording_hotkey = None;
                    self.shared_recording.store(false, Ordering::Relaxed);
                    self.hotkey_hint = None;
                    return;
                }
                if let Some(vk) = egui_key_to_vk(key) {
                    let candidate = HotkeyBinding {
                        ctrl: modifiers.ctrl,
                        alt: modifiers.alt,
                        shift: modifiers.shift,
                        win: false,
                        vk,
                    };
                    if !candidate.is_valid() {
                        self.hotkey_hint =
                            Some("Add Ctrl, Alt, or Shift (or use F1–F12)".to_string());
                        continue;
                    }
                    if let Some(conflict) = self.find_hotkey_conflict(target, candidate) {
                        self.hotkey_hint =
                            Some(format!("Already used by {conflict} hotkey"));
                        return;
                    }
                    self.recording_hotkey = None;
                    self.shared_recording.store(false, Ordering::Relaxed);
                    self.hotkey_hint = None;
                    match target {
                        HotkeyTarget::Capture => self.config.capture_hotkey = Some(candidate),
                        HotkeyTarget::Overlay => self.config.overlay_hotkey = Some(candidate),
                        HotkeyTarget::ClickThrough => {
                            self.config.click_through_hotkey = Some(candidate)
                        }
                    }
                    self.save_current_config();
                    let _ = self
                        .ctrl_tx
                        .send(CtrlCmd::UpdateHotkey(target, Some(candidate)));
                    return;
                }
            }
        }
    }

    fn flush_window_saves(&mut self) {
        let (sync_vis, sync_ct, reverts) = {
            let mut s = self.state.lock().unwrap();
            (
                s.overlay_sync_visible.take(),
                s.overlay_sync_click_through.take(),
                std::mem::take(&mut s.hotkey_revert),
            )
        };
        let mut config_changed = false;
        if let Some(vis) = sync_vis {
            if self.config.overlay.visible != vis {
                self.config.overlay.visible = vis;
                config_changed = true;
            }
        }
        if let Some(ct) = sync_ct {
            if self.config.overlay.click_through != ct {
                self.config.overlay.click_through = ct;
                config_changed = true;
            }
        }
        for (target, prev) in reverts {
            match target {
                HotkeyTarget::Capture => self.config.capture_hotkey = prev,
                HotkeyTarget::Overlay => self.config.overlay_hotkey = prev,
                HotkeyTarget::ClickThrough => self.config.click_through_hotkey = prev,
            }
            config_changed = true;
        }
        if let Some(ws) = query_window_state(self.hwnd_raw) {
            self.config.main_window = Some(ws);
        }
        if let Some(ov_ws) = self.overlay.as_ref().and_then(|o| o.current_window_state()) {
            self.config.overlay_window = Some(ov_ws);
        }
        if config_changed
            || self.config.main_window != self.last_saved_main
            || self.config.overlay_window != self.last_saved_overlay
        {
            self.save_current_config();
            self.last_saved_main = self.config.main_window;
            self.last_saved_overlay = self.config.overlay_window;
        }
    }
}

impl Drop for TranslateLiveApp {
    fn drop(&mut self) {
        self.flush_window_saves();
        let _ = self.ctrl_tx.send(CtrlCmd::ShutdownController);
        if let Some(join) = self.ctrl_join.take() {
            let _ = join.join();
        }
        if let Some(ov) = self.overlay.take() {
            ov.shutdown();
        }
        if let Some(shell) = self.shell.as_ref() {
            shell.shutdown();
        }
    }
}

impl eframe::App for TranslateLiveApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.track_window_geometries();

        if ctx.input(|i| i.viewport().close_requested()) {
            let force_quit = self.state.lock().unwrap().force_quit;
            if !force_quit && self.config.minimize_to_tray_on_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if let Some(shell) = self.shell.as_ref() {
                    shell.hide_window();
                }
            } else {
                self.flush_window_saves();
                let _ = self.ctrl_tx.send(CtrlCmd::ShutdownController);
            }
        }

        self.handle_hotkey_recording(ctx);
        self.telemetry_snap = self.telemetry.poll();

        let (
            phase,
            download,
            benchmarking,
            error,
            notice,
            active_asr_name,
            active_backend_desc,
            asr_ms,
            tl_ms,
            tok_per_sec,
            interim_original,
            interim_translation,
            picked_folder,
            picked_bg_image,
            hotkey_reverts,
            overlay_sync_visible,
            overlay_sync_click_through,
            display_changed,
            cache_refresh,
        ) = {
            let mut s = self.state.lock().unwrap();
            let cr = s.cache_refresh_needed;
            s.cache_refresh_needed = false;
            let dc = s.display_changed;
            s.display_changed = false;
            if s.transcript_version != self.ui_transcript_version {
                self.ui_transcript = s.transcript.clone();
                self.ui_transcript_version = s.transcript_version;
                self.ui_total_finalized = s.total_finalized_count;
            }
            (
                s.capture_phase.clone(),
                s.download.clone(),
                s.benchmarking,
                s.error.clone(),
                s.notice.clone(),
                s.active_asr_name.clone(),
                s.active_backend_desc.clone(),
                s.asr_ms,
                s.tl_ms,
                s.tok_per_sec,
                s.interim_original.clone(),
                s.interim_translation.clone(),
                s.picked_folder.take(),
                s.picked_bg_image.take(),
                std::mem::take(&mut s.hotkey_revert),
                s.overlay_sync_visible.take(),
                s.overlay_sync_click_through.take(),
                dc,
                cr,
            )
        };

        if cache_refresh {
            self.refresh_cache_entries();
        }
        if display_changed {
            self.recover_monitors_if_needed();
        }
        if let Some(folder) = picked_folder {
            self.apply_models_dir(folder);
        }
        if !hotkey_reverts.is_empty() {
            for (target, prev) in hotkey_reverts {
                match target {
                    HotkeyTarget::Capture => self.config.capture_hotkey = prev,
                    HotkeyTarget::Overlay => self.config.overlay_hotkey = prev,
                    HotkeyTarget::ClickThrough => self.config.click_through_hotkey = prev,
                }
            }
            self.save_current_config();
        }
        let mut synced_overlay_flags = false;
        if let Some(vis) = overlay_sync_visible {
            if self.config.overlay.visible != vis {
                self.config.overlay.visible = vis;
                synced_overlay_flags = true;
            }
        }
        if let Some(ct) = overlay_sync_click_through {
            if self.config.overlay.click_through != ct {
                self.config.overlay.click_through = ct;
                synced_overlay_flags = true;
            }
        }
        if synced_overlay_flags {
            self.save_current_config();
        }
        if let Some(img_path) = picked_bg_image {
            self.config.overlay.bg_image_path = Some(img_path.to_string_lossy().into_owned());
            self.save_current_config();
            self.sync_overlay_config();
        }

        let is_on = phase.is_on();
        if is_on != self.last_viewport_active {
            self.last_viewport_active = is_on;
            ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(Arc::new(
                icon::app_viewport_icon(is_on),
            ))));
        }

        if is_on || download.is_some() || phase.is_busy() || benchmarking {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else {
            ctx.request_repaint_after(Duration::from_millis(800));
        }

        if self.config.dark_mode != self.applied_dark_mode {
            apply_theme_visuals(ctx, self.config.dark_mode);
            set_window_dark_titlebar(self.hwnd_raw, self.config.dark_mode);
            self.applied_dark_mode = self.config.dark_mode;
        }
        let theme = ThemeColors::for_mode(self.config.dark_mode);

        let models_dir = self.current_models_dir();
        let status_key = (
            models_dir.clone(),
            self.asr_url_input.clone(),
            self.llm_url_input.clone(),
        );
        if self.cached_status_key.as_ref() != Some(&status_key) {
            self.cached_asr_status = inspect_asr_cache(&models_dir, &self.asr_url_input);
            self.cached_llm_status = inspect_llm_cache(&models_dir, &self.llm_url_input);
            self.cached_status_key = Some(status_key);
        }
        let asr_cache = self.cached_asr_status;
        let llm_cache = self.cached_llm_status;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_window)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 9.0);

                // 1. Header bar
                let models_ready = matches!(asr_cache, CacheStatus::Ready(_))
                    && matches!(llm_cache, CacheStatus::Ready(_));
                self.render_header(ui, ctx, &theme, &phase, &download, models_ready);

                // 2. Navigation Tabs
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (tab, label) in [
                        (ActiveTab::ControlModels, "Control & Models"),
                        (ActiveTab::AudioVad, "Audio & VAD"),
                        (ActiveTab::OverlayTranscript, "Overlay & Transcript"),
                        (ActiveTab::Cache, "Cache"),
                    ] {
                        let active = self.active_tab == tab;
                        let fill = if active {
                            theme.bg_callout_active
                        } else {
                            theme.bg_card
                        };
                        let stroke_col = if active {
                            theme.accent_fill
                        } else {
                            theme.border_subtle
                        };
                        let txt_col = if active {
                            theme.text_primary
                        } else {
                            theme.text_muted
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(label).size(12.0).strong().color(txt_col),
                                )
                                .fill(fill)
                                .stroke(egui::Stroke::new(1.0_f32, stroke_col))
                                .corner_radius(egui::CornerRadius::same(7))
                                .min_size(egui::vec2(0.0, 28.0)),
                            )
                            .clicked()
                        {
                            self.active_tab = tab;
                            if tab == ActiveTab::Cache {
                                self.refresh_cache_entries();
                            }
                        }
                    }
                });

                // 3. Footer reserved bottom-up, main scroll area fills middle
                egui::TopBottomPanel::bottom("tl_footer")
                    .frame(
                        egui::Frame::new()
                            .fill(theme.bg_window)
                            .inner_margin(egui::Margin::symmetric(0, 4)),
                    )
                    .show_inside(ui, |ui| {
                        self.render_footer(ui, &theme);
                    });

                egui::ScrollArea::vertical()
                    .id_salt("main_tab_scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);

                        if let Some(err_msg) = error.as_ref() {
                            render_error_banner(ui, &theme, err_msg, || {
                                self.state.lock().unwrap().error = None;
                            });
                        }

                        match self.active_tab {
                            ActiveTab::ControlModels => {
                                self.render_control_models_tab(
                                    ui,
                                    ctx,
                                    &theme,
                                    &phase,
                                    download.as_ref(),
                                    benchmarking,
                                    &notice,
                                    &active_asr_name,
                                    &active_backend_desc,
                                    asr_ms,
                                    tl_ms,
                                    tok_per_sec,
                                    asr_cache,
                                    llm_cache,
                                );
                            }
                            ActiveTab::AudioVad => {
                                self.render_audio_vad_tab(ui, &theme, &phase);
                            }
                            ActiveTab::OverlayTranscript => {
                                self.render_overlay_transcript_tab(
                                    ui,
                                    ctx,
                                    &theme,
                                    &interim_original,
                                    &interim_translation,
                                );
                            }
                            ActiveTab::Cache => {
                                self.render_cache_tab(
                                    ui,
                                    &theme,
                                    &phase,
                                    download.is_some(),
                                    benchmarking,
                                );
                            }
                        }
                    });
            });
    }
}

impl TranslateLiveApp {
    fn render_header(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        theme: &ThemeColors,
        phase: &CapturePhase,
        download: &Option<DownloadState>,
        models_ready: bool,
    ) {
        let is_on = phase.is_on();
        ui.horizontal(|ui| {
            let tex = if is_on {
                &self.icon_tex_on
            } else {
                &self.icon_tex_off
            };
            ui.image((tex.id(), egui::vec2(26.0, 26.0)));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Translate Live")
                    .size(17.5)
                    .strong()
                    .color(theme.text_primary),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (dot_color, status_text): (egui::Color32, String) = if let Some(dl) = download {
                    if let Some(tot) = dl.total.filter(|&t| t > 0) {
                        let pct = ((dl.downloaded as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
                        (theme.dot_busy, format!("Downloading {pct:.0}%"))
                    } else {
                        (theme.dot_busy, "Downloading...".to_string())
                    }
                } else {
                    match phase {
                        CapturePhase::Listening { .. } => (theme.dot_ready, "Listening".to_string()),
                        CapturePhase::Loading => (theme.dot_busy, "Loading...".to_string()),
                        CapturePhase::Stopping => (theme.dot_busy, "Stopping...".to_string()),
                        CapturePhase::Off if models_ready => (theme.dot_ready, "Ready".to_string()),
                        CapturePhase::Off => (theme.dot_off, "Setup".to_string()),
                    }
                };

                egui::Frame::new()
                    .fill(theme.bg_card)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(8.0, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(rect.center(), 4.0, dot_color);
                            ui.label(
                                egui::RichText::new(status_text)
                                    .size(12.0)
                                    .color(theme.text_primary),
                            );
                        });
                    });

                ui.add_space(6.0);
                if theme_switch(ui, self.config.dark_mode, theme).clicked() {
                    self.config.dark_mode = !self.config.dark_mode;
                    apply_theme_visuals(ctx, self.config.dark_mode);
                    set_window_dark_titlebar(self.hwnd_raw, self.config.dark_mode);
                    self.applied_dark_mode = self.config.dark_mode;
                    self.save_current_config();
                }
            });
        });
    }

    fn render_footer(&mut self, ui: &mut egui::Ui, theme: &ThemeColors) {
        ui.horizontal(|ui| {
            if ui
                .checkbox(
                    &mut self.config.minimize_to_tray_on_close,
                    egui::RichText::new("Minimize to tray on close")
                        .size(11.5)
                        .color(theme.text_muted),
                )
                .changed()
            {
                self.save_current_config();
            }

            ui.add_space(6.0);

            if ui
                .checkbox(
                    &mut self.config.show_notifications,
                    egui::RichText::new("Notifications")
                        .size(11.5)
                        .color(theme.text_muted),
                )
                .changed()
            {
                self.shared_show_notifications
                    .store(self.config.show_notifications, Ordering::Relaxed);
                self.save_current_config();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Hide to Tray")
                                .size(11.5)
                                .color(theme.text_muted),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    if let Some(shell) = self.shell.as_ref() {
                        shell.hide_window();
                    }
                }

                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Reset Window Positions")
                                .size(11.0)
                                .color(theme.text_muted),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                        .corner_radius(egui::CornerRadius::same(5)),
                    )
                    .clicked()
                {
                    self.reset_window_positions();
                }
            });
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn render_control_models_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        theme: &ThemeColors,
        phase: &CapturePhase,
        download: Option<&DownloadState>,
        benchmarking: bool,
        notice: &str,
        active_asr_name: &str,
        active_backend_desc: &str,
        asr_ms: Option<u128>,
        tl_ms: Option<u128>,
        tok_per_sec: Option<f32>,
        asr_cache: CacheStatus,
        llm_cache: CacheStatus,
    ) {
        let is_on = phase.is_on();
        let is_busy_cap = phase.is_busy();
        let is_downloading = download.is_some();

        // Card 1: Quick Controls, Live Audio & Telemetry Strip, Global Hotkeys
        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            let cap_label = match phase {
                CapturePhase::Off => "Live Capture (Start)",
                CapturePhase::Loading => "Loading ASR & GPU LLM...",
                CapturePhase::Listening { .. } => "Live Capture (Stop)",
                CapturePhase::Stopping => "Stopping Capture...",
            };
            if switch_row_widget(
                ui,
                theme,
                egui::Id::new("live_capture_switch"),
                44.0,
                13.5,
                cap_label,
                is_on || matches!(phase, CapturePhase::Loading),
                !is_busy_cap && !is_downloading && !benchmarking,
                is_busy_cap,
            )
            .clicked()
            {
                if is_on {
                    let _ = self.ctrl_tx.send(CtrlCmd::StopCapture);
                } else {
                    let _ = self.ctrl_tx.send(CtrlCmd::StartCapture {
                        models_dir: self.current_models_dir(),
                        asr_url: self.asr_url_input.clone(),
                        llm_url: self.llm_url_input.clone(),
                        audio_vad: self.config.audio_vad.clone(),
                    });
                }
            }

            if toggle_switch_row(
                ui,
                theme,
                egui::Id::new("show_overlay_switch"),
                "Show Subtitle Overlay",
                self.config.overlay.visible,
                true,
            )
            .clicked()
            {
                self.config.overlay.visible = !self.config.overlay.visible;
                self.save_current_config();
                self.sync_overlay_config();
            }

            ui.columns(2, |cols| {
                if toggle_switch_row(
                    &mut cols[0],
                    theme,
                    egui::Id::new("click_through_switch"),
                    "Click-Through",
                    self.config.overlay.click_through,
                    true,
                )
                .clicked()
                {
                    self.config.overlay.click_through = !self.config.overlay.click_through;
                    self.save_current_config();
                    self.sync_overlay_config();
                }

                if toggle_switch_row(
                    &mut cols[1],
                    theme,
                    egui::Id::new("top_most_switch"),
                    "Top-Most",
                    self.config.overlay.top_most,
                    true,
                )
                .clicked()
                {
                    self.config.overlay.top_most = !self.config.overlay.top_most;
                    self.save_current_config();
                    self.sync_overlay_config();
                }
            });

            // Live Audio & Telemetry Strip
            egui::Frame::new()
                .fill(theme.bg_callout)
                .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 6.0;

                    let rms = f32::from_bits(self.rms_bits.load(Ordering::Relaxed)).clamp(0.0, 1.0);
                    let vad_p = f32::from_bits(self.vad_prob_bits.load(Ordering::Relaxed)).clamp(0.0, 1.0);
                    let rms_pct = (rms * 100.0).clamp(0.0, 100.0);
                    let vad_pct = (vad_p * 100.0).clamp(0.0, 100.0);

                    ui.columns(2, |cols| {
                        draw_meter_bar(
                            &mut cols[0],
                            theme,
                            "RMS",
                            rms_pct,
                            Some(self.config.audio_vad.energy_threshold_pct),
                            theme.accent_fill,
                        );
                        draw_meter_bar(
                            &mut cols[1],
                            theme,
                            "VAD",
                            vad_pct,
                            Some(self.config.audio_vad.silero_threshold_pct),
                            theme.dot_busy,
                        );
                    });

                    let asr_str = asr_ms
                        .map(|ms| format!("{ms} ms"))
                        .unwrap_or_else(|| "—".to_string());
                    let tl_str = tl_ms
                        .map(|ms| format!("{ms} ms"))
                        .unwrap_or_else(|| "—".to_string());
                    let tps_str = tok_per_sec
                        .map(|t| format!("{t:.1}"))
                        .unwrap_or_else(|| "—".to_string());

                    ui.horizontal_wrapped(|ui| {
                        let asr_label = if is_on {
                            active_asr_name.to_string()
                        } else {
                            display_asr_name(&self.asr_url_input)
                        };
                        let dev_label = if is_on {
                            format!("{} ({active_backend_desc})", self.telemetry_snap.gpu_name)
                        } else {
                            self.telemetry_snap.gpu_name.clone()
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{asr_label}  ·  {dev_label}"
                            ))
                            .size(11.0)
                            .color(theme.text_muted),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "CPU {:.0}%  ·  RAM {:.0} MB  ·  GPU {:.0} MB  ·  ASR {asr_str}  ·  TL {tl_str}  ·  {tps_str} Tok/s",
                                self.telemetry_snap.cpu_pct,
                                self.telemetry_snap.ram_mb,
                                self.telemetry_snap.gpu_mb,
                            ))
                            .size(11.0)
                            .color(theme.text_primary),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let bench_lbl = if benchmarking {
                                "Testing..."
                            } else {
                                "Benchmark"
                            };
                            if ui
                                .add_enabled(
                                    !benchmarking && !is_downloading && !is_busy_cap && !is_on,
                                    egui::Button::new(
                                        egui::RichText::new(bench_lbl)
                                            .size(11.0)
                                            .color(theme.text_primary),
                                    )
                                    .fill(theme.btn_sec_bg)
                                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                                    .corner_radius(egui::CornerRadius::same(5))
                                    .min_size(egui::vec2(74.0, 22.0)),
                                )
                                .on_hover_text("Test cached ASR & LLM inference latency (when capture is stopped)")
                                .clicked()
                            {
                                let _ = self.ctrl_tx.send(CtrlCmd::RunBenchmark {
                                    models_dir: self.current_models_dir(),
                                    asr_url: self.asr_url_input.clone(),
                                    llm_url: self.llm_url_input.clone(),
                                    audio_vad: self.config.audio_vad.clone(),
                                });
                            }
                        });
                    });
                });

            // Global Hotkeys
            self.render_hotkey_section(ui, theme);

            ui.label(
                egui::RichText::new(notice)
                    .size(11.5)
                    .color(theme.text_muted),
            );
        });

        // Card 2: Shared Model Storage Folder Bar
        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 7.0);

            let is_custom_dir = self.config.model_dir.is_some();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Shared Model Storage Folder")
                        .size(12.5)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let tag = if is_custom_dir {
                        "Custom folder"
                    } else {
                        "App folder (.exe\\models)"
                    };
                    ui.label(egui::RichText::new(tag).size(11.0).color(theme.text_muted));
                });
            });

            ui.horizontal(|ui| {
                let btn_space = if is_custom_dir { 168.0 } else { 86.0 };
                let edit_w = (ui.available_width() - btn_space).max(120.0);
                let dir_resp = ui.add_enabled(
                    !is_downloading && !benchmarking,
                    egui::TextEdit::singleline(&mut self.dir_input)
                        .desired_width(edit_w)
                        .hint_text("Folder to save ASR and LLM models")
                        .margin(egui::Margin::symmetric(8, 5)),
                );
                if dir_resp.changed() {
                    let dir = self.current_models_dir();
                    self.config.model_dir = if dir == default_model_dir() {
                        None
                    } else {
                        Some(self.dir_input.trim().to_string())
                    };
                    *self.shared_models_dir.lock().unwrap() = dir;
                    self.save_current_config();
                    self.refresh_cache_entries();
                }

                let browsing = self.browsing_folder.load(Ordering::Relaxed);
                if ui
                    .add_enabled(
                        !is_downloading && !benchmarking && !browsing,
                        egui::Button::new(
                            egui::RichText::new("Browse...")
                                .size(11.5)
                                .color(theme.text_primary),
                        )
                        .fill(theme.btn_sec_bg)
                        .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                        .corner_radius(egui::CornerRadius::same(6))
                        .min_size(egui::vec2(76.0, 26.0)),
                    )
                    .clicked()
                {
                    self.browsing_folder.store(true, Ordering::Relaxed);
                    let flag = Arc::clone(&self.browsing_folder);
                    let state_c = Arc::clone(&self.state);
                    let ctx_c = ctx.clone();
                    let init_dir = self.current_models_dir();
                    if let Err(err) = thread::Builder::new()
                        .name("folder-picker".to_string())
                        .spawn(move || {
                            if let Some(chosen) = pick_folder_dialog(Some(&init_dir)) {
                                state_c.lock().unwrap().picked_folder = Some(chosen);
                            }
                            flag.store(false, Ordering::Relaxed);
                            ctx_c.request_repaint();
                        })
                    {
                        self.browsing_folder.store(false, Ordering::Relaxed);
                        self.state.lock().unwrap().error =
                            Some(format!("Failed to open folder picker: {err}"));
                    }
                }

                if is_custom_dir
                    && ui
                        .add_enabled(
                            !is_downloading && !benchmarking,
                            egui::Button::new(
                                egui::RichText::new("Reset Default")
                                    .size(11.5)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(6))
                            .min_size(egui::vec2(84.0, 26.0)),
                        )
                        .clicked()
                {
                    self.apply_models_dir(default_model_dir());
                }
            });
        });

        // Card 3: Voice Capture (ASR) Model Download Card
        self.render_asr_model_card(ui, theme, phase, download, benchmarking, asr_cache);

        // Card 4: Translation (LLM) Model Download Card
        self.render_llm_model_card(ui, theme, phase, download, benchmarking, llm_cache);
    }

    fn render_hotkey_section(&mut self, ui: &mut egui::Ui, theme: &ThemeColors) {
        let def_cap = Some(HotkeyBinding::function_key(8));
        match render_hotkey_row(
            ui,
            theme,
            self.config.capture_hotkey.map(|b| b.display_label()),
            "Live Capture",
            self.recording_hotkey == Some(HotkeyTarget::Capture),
            false,
            if self.config.capture_hotkey != def_cap {
                Some("Reset")
            } else if self.config.capture_hotkey.is_some() {
                Some("Clear")
            } else {
                None
            },
            "Change Hotkey",
        ) {
            HotkeyRowAction::ToggleRecord => self.toggle_recording(HotkeyTarget::Capture),
            HotkeyRowAction::Secondary => {
                if self.config.capture_hotkey != def_cap {
                    if let Some(b) = def_cap {
                        if let Some(conflict) = self.find_hotkey_conflict(HotkeyTarget::Capture, b) {
                            self.state.lock().unwrap().error = Some(format!(
                                "Cannot reset: {} is used by {conflict}",
                                b.display_label()
                            ));
                            return;
                        }
                    }
                    self.config.capture_hotkey = def_cap;
                } else {
                    self.config.capture_hotkey = None;
                }
                self.save_current_config();
                let _ = self
                    .ctrl_tx
                    .send(CtrlCmd::UpdateHotkey(HotkeyTarget::Capture, self.config.capture_hotkey));
            }
            HotkeyRowAction::None => {}
        }

        let def_ov = Some(HotkeyBinding::function_key(9));
        match render_hotkey_row(
            ui,
            theme,
            self.config.overlay_hotkey.map(|b| b.display_label()),
            "Subtitle Overlay",
            self.recording_hotkey == Some(HotkeyTarget::Overlay),
            false,
            if self.config.overlay_hotkey != def_ov {
                Some("Reset")
            } else if self.config.overlay_hotkey.is_some() {
                Some("Clear")
            } else {
                None
            },
            "Change Hotkey",
        ) {
            HotkeyRowAction::ToggleRecord => self.toggle_recording(HotkeyTarget::Overlay),
            HotkeyRowAction::Secondary => {
                if self.config.overlay_hotkey != def_ov {
                    if let Some(b) = def_ov {
                        if let Some(conflict) = self.find_hotkey_conflict(HotkeyTarget::Overlay, b) {
                            self.state.lock().unwrap().error = Some(format!(
                                "Cannot reset: {} is used by {conflict}",
                                b.display_label()
                            ));
                            return;
                        }
                    }
                    self.config.overlay_hotkey = def_ov;
                } else {
                    self.config.overlay_hotkey = None;
                }
                self.save_current_config();
                let _ = self
                    .ctrl_tx
                    .send(CtrlCmd::UpdateHotkey(HotkeyTarget::Overlay, self.config.overlay_hotkey));
            }
            HotkeyRowAction::None => {}
        }

        let def_ct = Some(HotkeyBinding::function_key(10));
        match render_hotkey_row(
            ui,
            theme,
            self.config.click_through_hotkey.map(|b| b.display_label()),
            "Click-Through",
            self.recording_hotkey == Some(HotkeyTarget::ClickThrough),
            false,
            if self.config.click_through_hotkey != def_ct {
                Some("Reset")
            } else if self.config.click_through_hotkey.is_some() {
                Some("Clear")
            } else {
                None
            },
            "Change Hotkey",
        ) {
            HotkeyRowAction::ToggleRecord => self.toggle_recording(HotkeyTarget::ClickThrough),
            HotkeyRowAction::Secondary => {
                if self.config.click_through_hotkey != def_ct {
                    if let Some(b) = def_ct {
                        if let Some(conflict) =
                            self.find_hotkey_conflict(HotkeyTarget::ClickThrough, b)
                        {
                            self.state.lock().unwrap().error = Some(format!(
                                "Cannot reset: {} is used by {conflict}",
                                b.display_label()
                            ));
                            return;
                        }
                    }
                    self.config.click_through_hotkey = def_ct;
                } else {
                    self.config.click_through_hotkey = None;
                }
                self.save_current_config();
                let _ = self.ctrl_tx.send(CtrlCmd::UpdateHotkey(
                    HotkeyTarget::ClickThrough,
                    self.config.click_through_hotkey,
                ));
            }
            HotkeyRowAction::None => {}
        }

        if let Some(hint) = &self.hotkey_hint {
            ui.label(egui::RichText::new(hint).size(11.0).color(theme.dot_busy));
        }
    }

    fn render_asr_model_card(
        &mut self,
        ui: &mut egui::Ui,
        theme: &ThemeColors,
        phase: &CapturePhase,
        download: Option<&DownloadState>,
        benchmarking: bool,
        asr_cache: CacheStatus,
    ) {
        let is_downloading_any = download.is_some();
        let is_downloading_this = download.is_some_and(|d| d.kind == ModelKind::Asr);
        let locked = is_downloading_any || phase.is_on() || phase.is_busy() || benchmarking;

        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 7.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Voice Capture (ASR) — Non-Whisper ONNX")
                        .size(12.5)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let status_text = if let Some(dl) = download.filter(|d| d.kind == ModelKind::Asr) {
                        if let Some(tot) = dl.total.filter(|&t| t > 0) {
                            let pct = ((dl.downloaded as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
                            format!("Downloading {pct:.0}%...")
                        } else {
                            "Downloading...".to_string()
                        }
                    } else {
                        match asr_cache {
                            CacheStatus::Ready(sz) => format!("Cached · {}", format_bytes(sz)),
                            CacheStatus::Partial(sz) => format!("Partial · {}", format_bytes(sz)),
                            CacheStatus::NotDownloaded => "Not Downloaded".to_string(),
                        }
                    };
                    ui.label(
                        egui::RichText::new(status_text)
                            .size(11.5)
                            .color(theme.text_muted),
                    );
                });
            });

            // Preset dropdown
            let matched_preset = find_asr_preset_by_url(&self.asr_url_input);
            let is_custom = self.asr_custom_mode || matched_preset.is_none();
            let selected_label = if is_custom {
                "Custom Hugging Face Link..."
            } else {
                matched_preset.unwrap().name
            };

            ui.add_enabled_ui(!locked, |ui| {
                egui::ComboBox::from_id_salt("asr_preset_combo")
                    .width(ui.available_width())
                    .height(340.0)
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        for preset in ASR_PRESETS {
                            let is_sel = !is_custom
                                && matched_preset.is_some_and(|m| m.url == preset.url);
                            if ui
                                .selectable_label(is_sel, format!("{} — {}", preset.name, preset.subtitle))
                                .clicked()
                            {
                                self.asr_custom_mode = false;
                                self.asr_url_input = preset.url.to_string();
                                self.config.asr_model_url = self.asr_url_input.clone();
                                *self.shared_asr_url.lock().unwrap() = self.asr_url_input.clone();
                                self.save_current_config();
                            }
                        }
                        ui.separator();
                        if ui
                            .selectable_label(
                                is_custom,
                                "Custom Hugging Face Link... — Paste any sherpa-onnx HF repo or .tar.bz2 URL",
                            )
                            .clicked()
                        {
                            self.asr_custom_mode = true;
                            self.focus_asr_url_next = true;
                            if !self.last_custom_asr_url.trim().is_empty() {
                                self.asr_url_input = self.last_custom_asr_url.clone();
                            } else if matched_preset.is_some() {
                                self.asr_url_input.clear();
                            }
                            *self.shared_asr_url.lock().unwrap() = self.asr_url_input.clone();
                            if let Ok(norm) = normalize_asr_url(&self.asr_url_input) {
                                self.config.asr_model_url = norm;
                                self.save_current_config();
                            }
                        }
                    });
            });

            let resp = ui.add_enabled(
                !locked,
                egui::TextEdit::singleline(&mut self.asr_url_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://huggingface.co/... (repo, .tar.bz2, or ONNX link)")
                    .margin(egui::Margin::symmetric(8, 6)),
            );
            if self.focus_asr_url_next {
                resp.request_focus();
                self.focus_asr_url_next = false;
            }
            if resp.changed() {
                if find_asr_preset_by_url(&self.asr_url_input).is_none() {
                    self.asr_custom_mode = true;
                    self.last_custom_asr_url = self.asr_url_input.clone();
                } else {
                    self.asr_custom_mode = false;
                }
                *self.shared_asr_url.lock().unwrap() = self.asr_url_input.clone();
                if let Ok(norm) = normalize_asr_url(&self.asr_url_input) {
                    self.config.asr_model_url = norm;
                    self.save_current_config();
                }
            }

            let preview_text = if self.asr_url_input.trim().is_empty() {
                "Paste a Hugging Face repo or .tar.bz2 link above".to_string()
            } else {
                format!("File: {}", display_asr_name(&self.asr_url_input))
            };
            ui.label(
                egui::RichText::new(preview_text)
                    .size(11.0)
                    .color(theme.text_muted),
            );

            if let Some(dl) = download.filter(|_| is_downloading_this) {
                render_download_progress_box(ui, theme, dl, &self.ctrl_tx);
            } else {
                ui.horizontal(|ui| {
                    let is_ready = matches!(asr_cache, CacheStatus::Ready(_));
                    let valid_url = normalize_asr_url(&self.asr_url_input).is_ok();
                    let btn_label = match asr_cache {
                        CacheStatus::Ready(_) => "Model Ready",
                        CacheStatus::Partial(_) => "Resume Download",
                        CacheStatus::NotDownloaded => "Download Model",
                    };
                    if ui
                        .add_enabled(
                            !locked && !is_ready && valid_url,
                            egui::Button::new(
                                egui::RichText::new(btn_label)
                                    .size(12.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(6))
                            .min_size(egui::vec2(130.0, 27.0)),
                        )
                        .clicked()
                    {
                        if let Ok(norm) = normalize_asr_url(&self.asr_url_input) {
                            self.asr_url_input = norm.clone();
                            self.config.asr_model_url = norm.clone();
                            *self.shared_asr_url.lock().unwrap() = norm;
                            self.save_current_config();
                        }
                        let _ = self.ctrl_tx.send(CtrlCmd::DownloadModel {
                            kind: ModelKind::Asr,
                            url: self.asr_url_input.clone(),
                            models_dir: self.current_models_dir(),
                        });
                    }

                    if ui
                        .add_enabled(
                            !locked && self.asr_url_input.trim() != DEFAULT_ASR_URL,
                            egui::Button::new(
                                egui::RichText::new("Reset Default")
                                    .size(12.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(6))
                            .min_size(egui::vec2(96.0, 27.0)),
                        )
                        .clicked()
                    {
                        self.asr_custom_mode = false;
                        self.asr_url_input = DEFAULT_ASR_URL.to_string();
                        self.config.asr_model_url = self.asr_url_input.clone();
                        *self.shared_asr_url.lock().unwrap() = self.asr_url_input.clone();
                        self.save_current_config();
                    }
                });
            }
        });
    }

    fn render_llm_model_card(
        &mut self,
        ui: &mut egui::Ui,
        theme: &ThemeColors,
        phase: &CapturePhase,
        download: Option<&DownloadState>,
        benchmarking: bool,
        llm_cache: CacheStatus,
    ) {
        let is_downloading_any = download.is_some();
        let is_downloading_this = download.is_some_and(|d| d.kind == ModelKind::Llm);
        let locked = is_downloading_any || phase.is_on() || phase.is_busy() || benchmarking;

        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 7.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Translation (LLM) — Local GGUF")
                        .size(12.5)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let status_text = if let Some(dl) = download.filter(|d| d.kind == ModelKind::Llm) {
                        if let Some(tot) = dl.total.filter(|&t| t > 0) {
                            let pct = ((dl.downloaded as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
                            format!("Downloading {pct:.0}%...")
                        } else {
                            "Downloading...".to_string()
                        }
                    } else {
                        match llm_cache {
                            CacheStatus::Ready(sz) => format!("Cached · {}", format_bytes(sz)),
                            CacheStatus::Partial(sz) => format!("Partial · {}", format_bytes(sz)),
                            CacheStatus::NotDownloaded => "Not Downloaded".to_string(),
                        }
                    };
                    ui.label(
                        egui::RichText::new(status_text)
                            .size(11.5)
                            .color(theme.text_muted),
                    );
                });
            });

            let matched_preset = LLM_PRESETS
                .iter()
                .find(|p| p.url.eq_ignore_ascii_case(self.llm_url_input.trim()));
            let is_custom = self.llm_custom_mode || matched_preset.is_none();
            let selected_label = if is_custom {
                "Custom Hugging Face Link..."
            } else {
                matched_preset.unwrap().name
            };

            ui.add_enabled_ui(!locked, |ui| {
                egui::ComboBox::from_id_salt("llm_preset_combo")
                    .width(ui.available_width())
                    .height(290.0)
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        for preset in LLM_PRESETS {
                            let is_sel = !is_custom
                                && matched_preset.is_some_and(|m| m.url == preset.url);
                            if ui
                                .selectable_label(is_sel, format!("{} — {}", preset.name, preset.subtitle))
                                .clicked()
                            {
                                self.llm_custom_mode = false;
                                self.llm_url_input = preset.url.to_string();
                                self.config.llm_model_url = self.llm_url_input.clone();
                                *self.shared_llm_url.lock().unwrap() = self.llm_url_input.clone();
                                self.save_current_config();
                            }
                        }
                        ui.separator();
                        if ui
                            .selectable_label(
                                is_custom,
                                "Custom Hugging Face Link... — Paste any Hugging Face GGUF download URL",
                            )
                            .clicked()
                        {
                            self.llm_custom_mode = true;
                            self.focus_llm_url_next = true;
                            if !self.last_custom_llm_url.trim().is_empty() {
                                self.llm_url_input = self.last_custom_llm_url.clone();
                            } else if matched_preset.is_some() {
                                self.llm_url_input.clear();
                            }
                            *self.shared_llm_url.lock().unwrap() = self.llm_url_input.clone();
                            if let Ok(norm) = normalize_hf_url(&self.llm_url_input) {
                                self.config.llm_model_url = norm;
                                self.save_current_config();
                            }
                        }
                    });
            });

            let resp = ui.add_enabled(
                !locked,
                egui::TextEdit::singleline(&mut self.llm_url_input)
                    .desired_width(f32::INFINITY)
                    .hint_text("https://huggingface.co/.../model.gguf")
                    .margin(egui::Margin::symmetric(8, 6)),
            );
            if self.focus_llm_url_next {
                resp.request_focus();
                self.focus_llm_url_next = false;
            }
            if resp.changed() {
                let is_preset = LLM_PRESETS
                    .iter()
                    .any(|p| p.url.eq_ignore_ascii_case(self.llm_url_input.trim()));
                if is_preset {
                    self.llm_custom_mode = false;
                } else {
                    self.llm_custom_mode = true;
                    self.last_custom_llm_url = self.llm_url_input.clone();
                }
                *self.shared_llm_url.lock().unwrap() = self.llm_url_input.clone();
                if let Ok(norm) = normalize_hf_url(&self.llm_url_input) {
                    self.config.llm_model_url = norm;
                    self.save_current_config();
                }
            }

            if self.llm_url_input.trim().is_empty() {
                ui.label(
                    egui::RichText::new("Paste a Hugging Face GGUF link above")
                        .size(11.0)
                        .color(theme.text_muted),
                );
            } else if let Ok(norm) = normalize_hf_url(&self.llm_url_input) {
                ui.label(
                    egui::RichText::new(format!("File: {}", display_llm_name(&norm)))
                        .size(11.0)
                        .color(theme.text_muted),
                );
            }

            if let Some(dl) = download.filter(|_| is_downloading_this) {
                render_download_progress_box(ui, theme, dl, &self.ctrl_tx);
            } else {
                ui.horizontal(|ui| {
                    let is_ready = matches!(llm_cache, CacheStatus::Ready(_));
                    let valid_url = normalize_hf_url(&self.llm_url_input).is_ok();
                    let btn_label = match llm_cache {
                        CacheStatus::Ready(_) => "Model Ready",
                        CacheStatus::Partial(_) => "Resume Download",
                        CacheStatus::NotDownloaded => "Download Model",
                    };
                    if ui
                        .add_enabled(
                            !locked && !is_ready && valid_url,
                            egui::Button::new(
                                egui::RichText::new(btn_label)
                                    .size(12.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(6))
                            .min_size(egui::vec2(130.0, 27.0)),
                        )
                        .clicked()
                    {
                        if let Ok(norm) = normalize_hf_url(&self.llm_url_input) {
                            self.llm_url_input = norm.clone();
                            self.config.llm_model_url = norm.clone();
                            *self.shared_llm_url.lock().unwrap() = norm;
                            self.save_current_config();
                        }
                        let _ = self.ctrl_tx.send(CtrlCmd::DownloadModel {
                            kind: ModelKind::Llm,
                            url: self.llm_url_input.clone(),
                            models_dir: self.current_models_dir(),
                        });
                    }

                    if ui
                        .add_enabled(
                            !locked && self.llm_url_input.trim() != DEFAULT_LLM_URL,
                            egui::Button::new(
                                egui::RichText::new("Reset Default")
                                    .size(12.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(6))
                            .min_size(egui::vec2(96.0, 27.0)),
                        )
                        .clicked()
                    {
                        self.llm_custom_mode = false;
                        self.llm_url_input = DEFAULT_LLM_URL.to_string();
                        self.config.llm_model_url = self.llm_url_input.clone();
                        *self.shared_llm_url.lock().unwrap() = self.llm_url_input.clone();
                        self.save_current_config();
                    }
                });
            }
        });
    }

    fn render_audio_vad_tab(
        &mut self,
        ui: &mut egui::Ui,
        theme: &ThemeColors,
        phase: &CapturePhase,
    ) {
        let locked_devices = phase.is_on() || phase.is_busy();
        let mut changed = false;

        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Audio Capture & Languages")
                        .size(13.0)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Reset to Default")
                                    .size(11.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.config.audio_vad = AudioVadConfig::default();
                        changed = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Refresh Devices")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.audio_devices = enumerate_audio_devices().unwrap_or_default();
                    }
                });
            });

            // WASAPI Loopback Output Device
            ui.add_enabled_ui(!locked_devices, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("WASAPI Loopback:").size(12.0).color(theme.text_muted));
                    let cur_lb_name = match self.config.audio_vad.loopback_device_id.as_deref() {
                        None => "System Default".to_string(),
                        Some(id) => self
                            .audio_devices
                            .outputs
                            .iter()
                            .find(|d| d.id == id)
                            .map(|d| d.name.clone())
                            .unwrap_or_else(|| "Custom Output".to_string()),
                    };
                    egui::ComboBox::from_id_salt("loopback_dev_combo")
                        .width(ui.available_width())
                        .selected_text(cur_lb_name)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    self.config.audio_vad.loopback_device_id.is_none(),
                                    "System Default",
                                )
                                .clicked()
                            {
                                self.config.audio_vad.loopback_device_id = None;
                                changed = true;
                            }
                            for dev in &self.audio_devices.outputs {
                                let label = if dev.is_default {
                                    format!("{} (Default)", dev.name)
                                } else {
                                    dev.name.clone()
                                };
                                let sel = self.config.audio_vad.loopback_device_id.as_deref()
                                    == Some(dev.id.as_str());
                                if ui.selectable_label(sel, label).clicked() {
                                    self.config.audio_vad.loopback_device_id = Some(dev.id.clone());
                                    changed = true;
                                }
                            }
                        });
                });

                // Optional Mic input
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Optional Mic:").size(12.0).color(theme.text_muted));
                    let cur_mic_name = match self.config.audio_vad.mic_device_id.as_deref() {
                        None => "Disabled".to_string(),
                        Some(DEFAULT_MIC_SENTINEL) => "System Default Mic".to_string(),
                        Some(id) => self
                            .audio_devices
                            .inputs
                            .iter()
                            .find(|d| d.id == id)
                            .map(|d| d.name.clone())
                            .unwrap_or_else(|| "Custom Mic".to_string()),
                    };
                    egui::ComboBox::from_id_salt("mic_dev_combo")
                        .width(ui.available_width())
                        .selected_text(cur_mic_name)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(
                                    self.config.audio_vad.mic_device_id.is_none(),
                                    "Disabled",
                                )
                                .clicked()
                            {
                                self.config.audio_vad.mic_device_id = None;
                                changed = true;
                            }
                            if ui
                                .selectable_label(
                                    self.config.audio_vad.mic_device_id.as_deref()
                                        == Some(DEFAULT_MIC_SENTINEL),
                                    "System Default Mic",
                                )
                                .clicked()
                            {
                                self.config.audio_vad.mic_device_id =
                                    Some(DEFAULT_MIC_SENTINEL.to_string());
                                changed = true;
                            }
                            for dev in &self.audio_devices.inputs {
                                let sel = self.config.audio_vad.mic_device_id.as_deref()
                                    == Some(dev.id.as_str());
                                if ui.selectable_label(sel, &dev.name).clicked() {
                                    self.config.audio_vad.mic_device_id = Some(dev.id.clone());
                                    changed = true;
                                }
                            }
                        });
                });

                // Compute Backend & ASR Threads
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("LLM Backend:").size(12.0).color(theme.text_muted));
                    egui::ComboBox::from_id_salt("compute_backend_combo")
                        .width(210.0)
                        .selected_text(self.config.audio_vad.compute_backend.label())
                        .show_ui(ui, |ui| {
                            for b in [ComputeBackend::VulkanGpu, ComputeBackend::CpuOnly] {
                                if ui
                                    .selectable_label(
                                        self.config.audio_vad.compute_backend == b,
                                        b.label(),
                                    )
                                    .clicked()
                                {
                                    self.config.audio_vad.compute_backend = b;
                                    changed = true;
                                }
                            }
                        });

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("ASR Threads:").size(12.0).color(theme.text_muted));
                    if ui
                        .add(egui::Slider::new(&mut self.config.audio_vad.asr_threads, 1..=12))
                        .changed()
                    {
                        changed = true;
                    }
                });
            });

            // Source & Target Languages + Context window
            ui.columns(2, |cols| {
                cols[0].horizontal(|ui| {
                    ui.label(egui::RichText::new("Source:").size(12.0).color(theme.text_muted));
                    let cur_src = SOURCE_LANGUAGES
                        .iter()
                        .find(|(c, _)| *c == self.config.audio_vad.source_lang)
                        .map(|(_, l)| *l)
                        .unwrap_or("ja - 日本語");
                    egui::ComboBox::from_id_salt("src_lang_combo")
                        .width(ui.available_width())
                        .selected_text(cur_src)
                        .show_ui(ui, |ui| {
                            for &(code, label) in SOURCE_LANGUAGES {
                                if ui
                                    .selectable_label(self.config.audio_vad.source_lang == code, label)
                                    .clicked()
                                {
                                    self.config.audio_vad.source_lang = code.to_string();
                                    changed = true;
                                }
                            }
                        });
                });

                cols[1].horizontal(|ui| {
                    ui.label(egui::RichText::new("Target:").size(12.0).color(theme.text_muted));
                    let cur_tgt = TARGET_LANGUAGES
                        .iter()
                        .find(|(c, _)| *c == self.config.audio_vad.target_lang)
                        .map(|(_, l)| *l)
                        .unwrap_or("en - English");
                    egui::ComboBox::from_id_salt("tgt_lang_combo")
                        .width(ui.available_width())
                        .selected_text(cur_tgt)
                        .show_ui(ui, |ui| {
                            for &(code, label) in TARGET_LANGUAGES {
                                if ui
                                    .selectable_label(self.config.audio_vad.target_lang == code, label)
                                    .clicked()
                                {
                                    self.config.audio_vad.target_lang = code.to_string();
                                    changed = true;
                                }
                            }
                        });
                });
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Rolling Context Window (turns):")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                if ui
                    .add(egui::Slider::new(&mut self.config.audio_vad.context_pairs, 0..=8))
                    .changed()
                {
                    changed = true;
                }
            });
        });

        // Card 2: VAD & Streaming Controls
        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("VAD Thresholds & Timing")
                        .size(13.0)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Gaming / BGM Stream")
                                    .size(11.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.config.audio_vad.apply_gaming_stream_profile();
                        changed = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("General / YouTube")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.config.audio_vad.apply_general_video_profile();
                        changed = true;
                    }
                });
            });

            // Live meters for interactive threshold tuning
            let rms = f32::from_bits(self.rms_bits.load(Ordering::Relaxed)).clamp(0.0, 1.0);
            let vad_p = f32::from_bits(self.vad_prob_bits.load(Ordering::Relaxed)).clamp(0.0, 1.0);
            ui.columns(2, |cols| {
                draw_meter_bar(
                    &mut cols[0],
                    theme,
                    "RMS",
                    rms * 100.0,
                    Some(self.config.audio_vad.energy_threshold_pct),
                    theme.accent_fill,
                );
                draw_meter_bar(
                    &mut cols[1],
                    theme,
                    "VAD",
                    vad_p * 100.0,
                    Some(self.config.audio_vad.silero_threshold_pct),
                    theme.dot_busy,
                );
            });

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("VAD Mode:").size(12.0).color(theme.text_muted));
                for mode in [VadMode::Silero, VadMode::Energy] {
                    if ui
                        .radio_value(&mut self.config.audio_vad.vad_mode, mode, mode.label())
                        .changed()
                    {
                        changed = true;
                    }
                }
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Silero VAD Threshold (%):")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                if ui
                    .add(
                        egui::Slider::new(&mut self.config.audio_vad.silero_threshold_pct, 5.0..=95.0)
                            .fixed_decimals(0),
                    )
                    .changed()
                {
                    changed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Energy Threshold (%):")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                if ui
                    .add(
                        egui::Slider::new(&mut self.config.audio_vad.energy_threshold_pct, 0.0..=40.0)
                            .fixed_decimals(1),
                    )
                    .changed()
                {
                    changed = true;
                }
            });

            ui.label(
                egui::RichText::new(
                    "Defaults tuned for VTuber/gaming streams: Energy Threshold filters quieter background voices/BGM in both VAD modes (0% = off).",
                )
                .size(11.0)
                .color(theme.text_muted),
            );

            ui.columns(2, |cols| {
                cols[0].horizontal(|ui| {
                    ui.label(egui::RichText::new("Min speech (s):").size(12.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.audio_vad.min_speech_sec)
                                .range(0.10..=3.0)
                                .speed(0.05)
                                .fixed_decimals(2),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                });
                cols[1].horizontal(|ui| {
                    ui.label(egui::RichText::new("Max speech (s):").size(12.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.audio_vad.max_speech_sec)
                                .range(1.5..=25.0)
                                .speed(0.25)
                                .fixed_decimals(2),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                });
            });

            ui.columns(2, |cols| {
                cols[0].horizontal(|ui| {
                    ui.label(egui::RichText::new("Silence:").size(12.0).color(theme.text_muted));
                    egui::ComboBox::from_id_salt("silence_mode_combo")
                        .width(105.0)
                        .selected_text(self.config.audio_vad.silence_mode.label())
                        .show_ui(ui, |ui| {
                            for m in [SilenceMode::Fixed, SilenceMode::Adaptive] {
                                if ui
                                    .selectable_label(
                                        self.config.audio_vad.silence_mode == m,
                                        m.label(),
                                    )
                                    .clicked()
                                {
                                    self.config.audio_vad.silence_mode = m;
                                    changed = true;
                                }
                            }
                        });
                });
                cols[1].horizontal(|ui| {
                    ui.label(egui::RichText::new("Silence dur (s):").size(12.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.audio_vad.silence_dur_sec)
                                .range(0.15..=3.0)
                                .speed(0.05)
                                .fixed_decimals(2),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                });
            });

            ui.horizontal(|ui| {
                if ui
                    .checkbox(
                        &mut self.config.audio_vad.incremental_asr,
                        egui::RichText::new("Live Sentence Fill (Incremental ASR)")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.add_space(10.0);
                ui.add_enabled_ui(self.config.audio_vad.incremental_asr, |ui| {
                    ui.label(egui::RichText::new("Interim interval (s):").size(12.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.audio_vad.interim_interval_sec)
                                .range(0.25..=5.0)
                                .speed(0.05)
                                .fixed_decimals(2),
                        )
                        .changed()
                    {
                        changed = true;
                    }
                });
            });

            ui.label(
                egui::RichText::new(
                    "Fills words into the sentence live as the speaker talks; a higher Silence dur keeps slow speakers in one sentence.",
                )
                .size(11.0)
                .color(theme.text_muted),
            );
        });

        if changed {
            *self.shared_audio_vad.lock().unwrap() = self.config.audio_vad.clone();
            self.save_current_config();
            let _ = self
                .ctrl_tx
                .send(CtrlCmd::UpdateAudioVad(self.config.audio_vad.clone()));
        }
    }

    fn render_overlay_transcript_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        theme: &ThemeColors,
        interim_original: &str,
        interim_translation: &str,
    ) {
        let mut ov_changed = false;

        // Card 1: Transparent Subtitle Overlay Settings
        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Subtitle Overlay Appearance")
                        .size(13.0)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Reset to Default")
                                    .size(11.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.config.overlay = OverlayConfig::default();
                        self.editing_line_id = None;
                        ov_changed = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Reset Window Positions")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.reset_window_positions();
                    }
                });
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Live Subtitle Style:")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                egui::ComboBox::from_id_salt("live_subtitle_style_combo")
                    .width(235.0)
                    .selected_text(self.config.overlay.live_style.label())
                    .show_ui(ui, |ui| {
                        for style in [
                            LiveSubtitleStyle::PrefixLockedFill,
                            LiveSubtitleStyle::CleanSentencesOnly,
                        ] {
                            if ui
                                .selectable_label(
                                    self.config.overlay.live_style == style,
                                    style.label(),
                                )
                                .clicked()
                            {
                                self.config.overlay.live_style = style;
                                ov_changed = true;
                            }
                        }
                    });
            });

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Visible translations:")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                if ui
                    .add(
                        egui::DragValue::new(&mut self.config.overlay.max_lines)
                            .range(1..=4)
                            .speed(0.1),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
                ui.add_space(10.0);
                if ui
                    .checkbox(
                        &mut self.config.overlay.smooth_scroll,
                        egui::RichText::new("Smooth scroll-up")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Min read time (s):")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                if ui
                    .add(
                        egui::DragValue::new(&mut self.config.overlay.min_display_sec)
                            .range(0.0..=4.0)
                            .speed(0.10)
                            .fixed_decimals(2),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
            });

            ui.label(
                egui::RichText::new(
                    "Shows up to 3–4 recent translations that smoothly glide up and gently recede as new speech translates live below (zero flash when finalizing); Min read time paces rapid back-to-back sentences (0 = off).",
                )
                .size(11.0)
                .color(theme.text_muted),
            );

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Background Opacity (%):").size(12.0).color(theme.text_muted));
                if ui
                    .add(
                        egui::Slider::new(&mut self.config.overlay.bg_opacity_pct, 0.0..=100.0)
                            .fixed_decimals(0),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
            });

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Background Color:").size(12.0).color(theme.text_muted));
                if color_hex_picker(ui, &mut self.config.overlay.bg_color_hex) {
                    ov_changed = true;
                }
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Corner Radius:").size(12.0).color(theme.text_muted));
                if ui
                    .add(
                        egui::Slider::new(&mut self.config.overlay.border_radius, 0.0..=32.0)
                            .fixed_decimals(0),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
            });

            // Background Image
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Background Image:").size(12.0).color(theme.text_muted));
                let img_label = self
                    .config
                    .overlay
                    .bg_image_path
                    .as_deref()
                    .and_then(|p| Path::new(p).file_name().and_then(|s| s.to_str()))
                    .unwrap_or("None");
                ui.label(egui::RichText::new(img_label).size(11.5).color(theme.text_primary));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.config.overlay.bg_image_path.is_some()
                        && ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Clear").size(11.0).color(theme.text_muted),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                                .corner_radius(egui::CornerRadius::same(5)),
                            )
                            .clicked()
                    {
                        self.config.overlay.bg_image_path = None;
                        ov_changed = true;
                    }

                    let browsing = self.browsing_image.load(Ordering::Relaxed);
                    if ui
                        .add_enabled(
                            !browsing,
                            egui::Button::new(
                                egui::RichText::new("Select...")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.browsing_image.store(true, Ordering::Relaxed);
                        let flag = Arc::clone(&self.browsing_image);
                        let state_c = Arc::clone(&self.state);
                        let ctx_c = ctx.clone();
                        let init_p = self.config.overlay.bg_image_path.clone().map(PathBuf::from);
                        if let Err(err) = thread::Builder::new()
                            .name("image-picker".to_string())
                            .spawn(move || {
                                if let Some(chosen) = pick_image_file_dialog(init_p.as_deref()) {
                                    state_c.lock().unwrap().picked_bg_image = Some(chosen);
                                }
                                flag.store(false, Ordering::Relaxed);
                                ctx_c.request_repaint();
                            })
                        {
                            self.browsing_image.store(false, Ordering::Relaxed);
                            self.state.lock().unwrap().error =
                                Some(format!("Failed to open image picker: {err}"));
                        }
                    }
                });
            });

            // Auto-hide
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Auto-Hide (s, 0=off):").size(12.0).color(theme.text_muted));
                if ui
                    .add(
                        egui::DragValue::new(&mut self.config.overlay.auto_hide_sec)
                            .range(0.0..=60.0)
                            .speed(0.5)
                            .fixed_decimals(1),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Anim:").size(12.0).color(theme.text_muted));
                egui::ComboBox::from_id_salt("hide_anim_combo")
                    .width(72.0)
                    .selected_text(self.config.overlay.hide_animation.label())
                    .show_ui(ui, |ui| {
                        for a in [HideAnimation::Fade, HideAnimation::None] {
                            if ui
                                .selectable_label(self.config.overlay.hide_animation == a, a.label())
                                .clicked()
                            {
                                self.config.overlay.hide_animation = a;
                                ov_changed = true;
                            }
                        }
                    });
                ui.label(egui::RichText::new("ms:").size(12.0).color(theme.text_muted));
                if ui
                    .add(
                        egui::DragValue::new(&mut self.config.overlay.hide_duration_ms)
                            .range(50..=1500)
                            .speed(25),
                    )
                    .changed()
                {
                    ov_changed = true;
                }
            });

            ui.separator();

            // Display lines list (Add / Edit / Remove / Reorder)
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Subtitle Text Lines")
                        .size(12.5)
                        .strong()
                        .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("+ Add Line")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        let next_id = self
                            .config
                            .overlay
                            .lines
                            .iter()
                            .map(|l| l.id)
                            .max()
                            .unwrap_or(0)
                            + 1;
                        self.config.overlay.lines.push(SubtitleLineConfig {
                            id: next_id,
                            label: format!("Line {next_id}"),
                            source: SubtitleLineSource::Translation,
                            visible: true,
                            font_family: "Segoe UI".to_string(),
                            font_size: 20.0,
                            color_hex: "#FFFFFF".to_string(),
                            align: TextAlign::Center,
                            outline: true,
                        });
                        self.editing_line_id = Some(next_id);
                        ov_changed = true;
                    }
                });
            });

            let len = self.config.overlay.lines.len();
            let mut swap_pair: Option<(usize, usize)> = None;
            let mut remove_idx: Option<usize> = None;

            for idx in 0..len {
                let line_id = self.config.overlay.lines[idx].id;
                let is_editing = self.editing_line_id == Some(line_id);
                egui::Frame::new()
                    .fill(theme.bg_callout)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            if ui
                                .checkbox(&mut self.config.overlay.lines[idx].visible, "")
                                .changed()
                            {
                                ov_changed = true;
                            }
                            let line = &self.config.overlay.lines[idx];
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} ({}) · {} · {:.0}px",
                                    line.label,
                                    line.source.label(),
                                    line.font_family,
                                    line.font_size
                                ))
                                .size(11.5)
                                .color(theme.text_primary),
                            );

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if len > 1
                                    && ui
                                        .small_button("×")
                                        .on_hover_text("Remove line")
                                        .clicked()
                                {
                                    remove_idx = Some(idx);
                                }
                                if ui.small_button(if is_editing { "Done" } else { "Edit" }).clicked()
                                {
                                    self.editing_line_id =
                                        if is_editing { None } else { Some(line_id) };
                                }
                                if idx + 1 < len && ui.small_button("▼").clicked() {
                                    swap_pair = Some((idx, idx + 1));
                                }
                                if idx > 0 && ui.small_button("▲").clicked() {
                                    swap_pair = Some((idx, idx - 1));
                                }
                            });
                        });

                        if is_editing {
                            let line = &mut self.config.overlay.lines[idx];
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Label:").size(11.0).color(theme.text_muted));
                                if ui
                                    .add(egui::TextEdit::singleline(&mut line.label).desired_width(90.0))
                                    .changed()
                                {
                                    ov_changed = true;
                                }
                                ui.label(egui::RichText::new("Source:").size(11.0).color(theme.text_muted));
                                egui::ComboBox::from_id_salt(("line_src", line.id))
                                    .width(95.0)
                                    .selected_text(line.source.label())
                                    .show_ui(ui, |ui| {
                                        for s in [
                                            SubtitleLineSource::Original,
                                            SubtitleLineSource::Translation,
                                        ] {
                                            if ui
                                                .selectable_label(line.source == s, s.label())
                                                .clicked()
                                            {
                                                line.source = s;
                                                ov_changed = true;
                                            }
                                        }
                                    });
                                if ui
                                    .checkbox(
                                        &mut line.outline,
                                        egui::RichText::new("Outline").size(11.0).color(theme.text_primary),
                                    )
                                    .changed()
                                {
                                    ov_changed = true;
                                }
                            });

                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Font:").size(11.0).color(theme.text_muted));
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut line.font_family)
                                            .desired_width(115.0),
                                    )
                                    .changed()
                                {
                                    ov_changed = true;
                                }
                                ui.label(egui::RichText::new("Size:").size(11.0).color(theme.text_muted));
                                if ui
                                    .add(
                                        egui::DragValue::new(&mut line.font_size)
                                            .range(10.0..=64.0)
                                            .speed(0.5)
                                            .fixed_decimals(0),
                                    )
                                    .changed()
                                {
                                    ov_changed = true;
                                }
                                if color_hex_picker(ui, &mut line.color_hex) {
                                    ov_changed = true;
                                }
                                egui::ComboBox::from_id_salt(("line_align", line.id))
                                    .width(68.0)
                                    .selected_text(line.align.label())
                                    .show_ui(ui, |ui| {
                                        for a in [TextAlign::Left, TextAlign::Center, TextAlign::Right]
                                        {
                                            if ui
                                                .selectable_label(line.align == a, a.label())
                                                .clicked()
                                            {
                                                line.align = a;
                                                ov_changed = true;
                                            }
                                        }
                                    });
                            });
                        }
                    });
            }

            if let Some((a, b)) = swap_pair {
                self.config.overlay.lines.swap(a, b);
                ov_changed = true;
            }
            if let Some(rm) = remove_idx {
                self.config.overlay.lines.remove(rm);
                if self.config.overlay.lines.is_empty() {
                    self.config.overlay.lines = default_subtitle_lines();
                }
                self.editing_line_id = None;
                ov_changed = true;
            }
        });

        if ov_changed {
            self.save_current_config();
            self.sync_overlay_config();
        }

        // Card 2: Live Transcript Feed
        let mut tr_changed = false;
        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 7.0);

            ui.horizontal(|ui| {
                if ui
                    .checkbox(
                        &mut self.config.transcript.visible,
                        egui::RichText::new("Live Transcript Feed")
                            .size(13.0)
                            .strong()
                            .color(theme.text_primary),
                    )
                    .changed()
                {
                    tr_changed = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Reset to Default")
                                    .size(11.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.config.transcript = TranscriptConfig::default();
                        tr_changed = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Clear")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        let _ = self.ctrl_tx.send(CtrlCmd::ClearTranscript);
                    }
                    if ui
                        .checkbox(
                            &mut self.config.transcript.auto_scroll,
                            egui::RichText::new("Auto-scroll")
                                .size(11.5)
                                .color(theme.text_muted),
                        )
                        .changed()
                    {
                        tr_changed = true;
                    }
                });
            });

            if self.config.transcript.visible {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Box:").size(11.0).color(theme.text_muted));
                    if color_hex_picker(ui, &mut self.config.transcript.container_color_hex) {
                        tr_changed = true;
                    }
                    ui.label(egui::RichText::new("Header:").size(11.0).color(theme.text_muted));
                    if color_hex_picker(ui, &mut self.config.transcript.header_color_hex) {
                        tr_changed = true;
                    }
                    ui.label(egui::RichText::new("Opacity:").size(11.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.transcript.container_opacity_pct)
                                .range(20.0..=100.0)
                                .speed(2.0)
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        tr_changed = true;
                    }
                    ui.label(egui::RichText::new("Font:").size(11.0).color(theme.text_muted));
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.transcript.font_size)
                                .range(10.0..=24.0)
                                .speed(0.5)
                                .fixed_decimals(0),
                        )
                        .changed()
                    {
                        tr_changed = true;
                    }
                });

                let alpha = ((self.config.transcript.container_opacity_pct / 100.0) * 255.0)
                    .round()
                    .clamp(25.0, 255.0) as u8;
                let [cr, cg, cb] =
                    parse_hex_color(&self.config.transcript.container_color_hex).unwrap_or([24, 24, 24]);
                let [hr, hg, hb] =
                    parse_hex_color(&self.config.transcript.header_color_hex).unwrap_or([34, 34, 34]);

                let box_bg = egui::Color32::from_rgba_unmultiplied(cr, cg, cb, alpha);
                let hdr_bg = egui::Color32::from_rgba_unmultiplied(hr, hg, hb, alpha);
                let fsz = self.config.transcript.font_size;
                let new_items = self.ui_total_finalized != self.last_finalized_seen;
                self.last_finalized_seen = self.ui_total_finalized;

                egui::Frame::new()
                    .fill(box_bg)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Frame::new()
                            .fill(hdr_bg)
                            .corner_radius(egui::CornerRadius::same(5))
                            .inner_margin(egui::Margin::symmetric(8, 4))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} utterances captured",
                                        self.ui_total_finalized
                                    ))
                                    .size(11.0)
                                    .color(theme.text_muted),
                                );
                            });

                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .id_salt("transcript_feed_scroll")
                            .max_height(210.0)
                            .auto_shrink([false, false])
                            .stick_to_bottom(self.config.transcript.auto_scroll)
                            .show(ui, |ui| {
                                if self.ui_transcript.is_empty() && interim_original.is_empty() {
                                    ui.label(
                                        egui::RichText::new("Waiting for live speech...")
                                            .size(fsz)
                                            .color(theme.text_muted),
                                    );
                                }
                                for entry in &self.ui_transcript {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "[{}] {}",
                                            entry.timestamp, entry.original
                                        ))
                                        .size((fsz - 1.0).max(10.0))
                                        .color(theme.text_muted),
                                    );
                                    if !entry.translation.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&entry.translation)
                                                .size(fsz)
                                                .color(theme.text_primary),
                                        );
                                    }
                                    ui.add_space(4.0);
                                }
                                if !interim_original.is_empty() {
                                    ui.label(
                                        egui::RichText::new(format!("[LIVE] {interim_original}"))
                                            .size((fsz - 1.0).max(10.0))
                                            .color(theme.dot_busy),
                                    );
                                    if !interim_translation.is_empty() {
                                        ui.label(
                                            egui::RichText::new(interim_translation)
                                                .size(fsz)
                                                .color(theme.text_primary),
                                        );
                                    }
                                }
                                if new_items && self.config.transcript.auto_scroll {
                                    ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                                }
                            });
                    });
            }
        });

        if tr_changed {
            self.save_current_config();
        }
    }

    fn render_cache_tab(
        &mut self,
        ui: &mut egui::Ui,
        theme: &ThemeColors,
        phase: &CapturePhase,
        is_downloading: bool,
        benchmarking: bool,
    ) {
        let locked = phase.is_on() || phase.is_busy() || is_downloading || benchmarking;
        let models_dir = self.current_models_dir();
        let total_size: u64 = self.cache_entries.iter().map(|e| e.size_bytes).sum();

        card_frame(theme).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Model Cache ({})",
                        format_bytes(total_size)
                    ))
                    .size(13.0)
                    .strong()
                    .color(theme.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Refresh")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        self.refresh_cache_entries();
                    }

                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Open Folder")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        open_folder_in_explorer(&models_dir);
                    }

                    if ui
                        .add_enabled(
                            !locked,
                            egui::Button::new(
                                egui::RichText::new("Clean Partials")
                                    .size(11.0)
                                    .color(theme.text_muted),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        match clean_partial_files(&models_dir) {
                            Ok(freed) => {
                                self.state.lock().unwrap().notice =
                                    format!("Cleaned partial downloads ({})", format_bytes(freed));
                                self.refresh_cache_entries();
                            }
                            Err(e) => {
                                self.state.lock().unwrap().error = Some(e.to_string());
                            }
                        }
                    }
                });
            });

            ui.label(
                egui::RichText::new(format!("Location: {}", models_dir.display()))
                    .size(11.0)
                    .color(theme.text_muted),
            );

            if self.cache_entries.is_empty() {
                egui::Frame::new()
                    .fill(theme.bg_callout)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(
                                "No models cached yet. Nothing is downloaded until you click Download Model.",
                            )
                            .size(12.0)
                            .color(theme.text_muted),
                        );
                    });
            } else {
                let mut delete_target: Option<PathBuf> = None;
                for entry in &self.cache_entries {
                    egui::Frame::new()
                        .fill(theme.bg_callout)
                        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                        .corner_radius(egui::CornerRadius::same(7))
                        .inner_margin(egui::Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let badge_col = if entry.is_partial {
                                    theme.dot_busy
                                } else {
                                    theme.accent_fill
                                };
                                egui::Frame::new()
                                    .fill(theme.bg_keycap)
                                    .stroke(egui::Stroke::new(1.0_f32, badge_col))
                                    .corner_radius(egui::CornerRadius::same(4))
                                    .inner_margin(egui::Margin::symmetric(6, 2))
                                    .show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(&entry.category)
                                                .size(10.5)
                                                .strong()
                                                .color(theme.text_primary),
                                        );
                                    });

                                ui.label(
                                    egui::RichText::new(&entry.name)
                                        .size(12.0)
                                        .color(theme.text_primary),
                                );

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(
                                                !locked,
                                                egui::Button::new(
                                                    egui::RichText::new("Delete")
                                                        .size(11.0)
                                                        .color(theme.error_text),
                                                )
                                                .fill(egui::Color32::TRANSPARENT)
                                                .stroke(egui::Stroke::new(
                                                    1.0_f32,
                                                    theme.border_subtle,
                                                ))
                                                .corner_radius(egui::CornerRadius::same(5)),
                                            )
                                            .clicked()
                                        {
                                            delete_target = Some(entry.path.clone());
                                        }
                                        ui.label(
                                            egui::RichText::new(format_bytes(entry.size_bytes))
                                                .size(11.5)
                                                .color(theme.text_muted),
                                        );
                                    },
                                );
                            });
                        });
                }
                if let Some(p) = delete_target {
                    if let Err(e) = delete_cache_entry(&p) {
                        self.state.lock().unwrap().error = Some(e.to_string());
                    } else {
                        self.refresh_cache_entries();
                    }
                }
            }
        });
    }
}

fn color_hex_picker(ui: &mut egui::Ui, hex: &mut String) -> bool {
    let mut changed = false;
    let rgb = parse_hex_color(hex).unwrap_or([255, 255, 255]);
    let mut c32 = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    if ui.color_edit_button_srgba(&mut c32).changed() {
        *hex = format_hex_color([c32.r(), c32.g(), c32.b()]);
        changed = true;
    }
    if ui
        .add(egui::TextEdit::singleline(hex).desired_width(64.0))
        .changed()
    {
        if parse_hex_color(hex).is_some() {
            changed = true;
        }
    }
    changed
}

fn render_error_banner(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    err_msg: &str,
    on_dismiss: impl FnOnce(),
) {
    egui::Frame::new()
        .fill(theme.error_bg)
        .stroke(egui::Stroke::new(1.0_f32, theme.error_border))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(err_msg)
                        .size(11.5)
                        .color(theme.error_text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("×").clicked() {
                        on_dismiss();
                    }
                });
            });
        });
}

fn render_download_progress_box(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    dl: &DownloadState,
    ctrl_tx: &Sender<CtrlCmd>,
) {
    egui::Frame::new()
        .fill(theme.bg_callout)
        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;

            ui.horizontal(|ui| {
                ui.spinner();
                let heading = if dl.downloaded == 0 {
                    format!("Connecting ({})...", shorten_middle(&dl.label, 34))
                } else {
                    format!("Downloading {}...", shorten_middle(&dl.label, 34))
                };
                ui.label(
                    egui::RichText::new(heading)
                        .size(12.0)
                        .strong()
                        .color(theme.text_primary),
                );
                if let Some(tot) = dl.total.filter(|&t| t > 0) {
                    let pct = ((dl.downloaded as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(format!("{pct:.0}%"))
                                .size(12.0)
                                .strong()
                                .color(theme.text_primary),
                        );
                    });
                }
            });

            let frac = dl
                .total
                .filter(|&t| t > 0)
                .map(|t| (dl.downloaded as f32 / t as f32).clamp(0.0, 1.0))
                .unwrap_or(0.0);
            draw_neutral_progress_bar(ui, theme, frac, dl.total.is_none());

            let mut stats = if let Some(tot) = dl.total {
                format!(
                    "{} / {}  ·  {}",
                    format_bytes(dl.downloaded),
                    format_bytes(tot),
                    format_speed(dl.speed_bps)
                )
            } else {
                format!(
                    "{}  ·  {}",
                    format_bytes(dl.downloaded),
                    format_speed(dl.speed_bps)
                )
            };
            if let Some(eta) = format_eta(dl.downloaded, dl.total, dl.speed_bps) {
                stats.push_str(&format!("  ·  {eta}"));
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(stats).size(11.0).color(theme.text_primary));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Pause / Cancel")
                                    .size(11.0)
                                    .color(theme.text_primary),
                            )
                            .fill(theme.btn_sec_bg)
                            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                            .corner_radius(egui::CornerRadius::same(5)),
                        )
                        .clicked()
                    {
                        let _ = ctrl_tx.send(CtrlCmd::CancelDownload);
                    }
                });
            });
        });
}

fn draw_meter_bar(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    label: &str,
    pct: f32,
    threshold_pct: Option<f32>,
    fill_col: egui::Color32,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            egui::RichText::new(format!("{label} {:>3.0}%", pct.clamp(0.0, 100.0)))
                .size(11.0)
                .monospace()
                .color(theme.text_muted),
        );
        let w = ui.available_width().max(60.0);
        let h = 8.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        let rad = egui::CornerRadius::same(4);
        ui.painter().rect_filled(rect, rad, theme.bg_track);
        let frac = (pct / 100.0).clamp(0.0, 1.0);
        if frac > 0.005 {
            let fw = (w * frac).max(4.0).min(w);
            let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fw, h));
            ui.painter().rect_filled(fill_rect, rad, fill_col);
        }
        if let Some(th) = threshold_pct {
            let tx = rect.min.x + w * (th / 100.0).clamp(0.0, 1.0);
            ui.painter().line_segment(
                [egui::pos2(tx, rect.min.y - 1.0), egui::pos2(tx, rect.max.y + 1.0)],
                egui::Stroke::new(1.5_f32, theme.text_primary),
            );
        }
    });
}

fn draw_neutral_progress_bar(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    frac: f32,
    indeterminate: bool,
) {
    let width = ui.available_width();
    let height = 9.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let radius = egui::CornerRadius::same(4);
    ui.painter().rect_filled(rect, radius, theme.bg_track);

    if indeterminate {
        ui.ctx().request_repaint();
        let t = ui.input(|i| i.time) as f32;
        let seg_w = (width * 0.28).max(36.0);
        let travel = (width - seg_w).max(1.0);
        let pos = ((t * 0.9).sin() * 0.5 + 0.5) * travel;
        let fill_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + pos, rect.min.y),
            egui::vec2(seg_w, height),
        );
        ui.painter().rect_filled(fill_rect, radius, theme.accent_fill);
    } else if frac > 0.001 {
        let fill_w = (width * frac.clamp(0.0, 1.0)).max(height);
        let fill_rect =
            egui::Rect::from_min_size(rect.min, egui::vec2(fill_w.min(width), height));
        ui.painter().rect_filled(fill_rect, radius, theme.accent_fill);
    }
}

fn card_frame(theme: &ThemeColors) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_card)
        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(13))
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        ((x as f32) + ((y as f32) - (x as f32)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    egui::Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

fn draw_sun_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: egui::Color32) {
    painter.circle_filled(center, radius * 0.43, color);
    let inner_r = radius * 0.67;
    let outer_r = radius;
    let stroke = egui::Stroke::new(1.3_f32, color);
    for i in 0..8 {
        let angle = (i as f32) * std::f32::consts::FRAC_PI_4;
        let (s, c) = angle.sin_cos();
        let p0 = egui::pos2(center.x + c * inner_r, center.y + s * inner_r);
        let p1 = egui::pos2(center.x + c * outer_r, center.y + s * outer_r);
        painter.line_segment([p0, p1], stroke);
    }
}

fn draw_moon_icon(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    color: egui::Color32,
    cutout_bg: egui::Color32,
) {
    painter.circle_filled(center, radius * 0.88, color);
    let cutout_center = egui::pos2(center.x + radius * 0.34, center.y - radius * 0.22);
    painter.circle_filled(cutout_center, radius * 0.78, cutout_bg);
}

fn theme_switch(ui: &mut egui::Ui, is_dark: bool, theme: &ThemeColors) -> egui::Response {
    let desired_size = egui::vec2(52.0, 26.0);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let tooltip = if is_dark {
        "Switch to Light Mode"
    } else {
        "Switch to Dark Mode"
    };
    let response = response.on_hover_text(tooltip);

    let t = ui.ctx().animate_bool_with_time(response.id, is_dark, 0.16);
    let track_light = egui::Color32::WHITE;
    let track_dark = egui::Color32::from_rgb(42, 42, 42);
    let border_light = egui::Color32::from_rgb(200, 196, 190);
    let border_dark = egui::Color32::from_rgb(76, 76, 76);

    let track_fill = lerp_color(track_light, track_dark, t);
    let mut track_border = lerp_color(border_light, border_dark, t);
    if response.hovered() {
        track_border = lerp_color(track_border, theme.text_muted, 0.35);
    }

    let radius = egui::CornerRadius::same(13);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track_fill);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.2_f32, track_border),
        egui::StrokeKind::Inside,
    );

    let cy = rect.center().y;
    let left_slot = egui::pos2(rect.left() + 13.0, cy);
    let right_slot = egui::pos2(rect.right() - 13.0, cy);

    if t > 0.05 {
        let sun_muted = lerp_color(track_fill, egui::Color32::from_rgb(140, 140, 140), t);
        draw_sun_icon(painter, left_slot, 5.4, sun_muted);
    }
    if t < 0.95 {
        let moon_muted = lerp_color(track_fill, egui::Color32::from_rgb(158, 158, 158), 1.0 - t);
        draw_moon_icon(painter, right_slot, 5.2, moon_muted, track_fill);
    }

    let knob_x = egui::lerp(left_slot.x..=right_slot.x, t);
    let knob_center = egui::pos2(knob_x, cy);
    painter.circle_filled(
        egui::pos2(knob_center.x, knob_center.y + 1.0),
        10.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 24),
    );
    let knob_fill = egui::Color32::WHITE;
    painter.circle_filled(knob_center, 9.8, knob_fill);
    painter.circle_stroke(
        knob_center,
        9.8,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(214, 210, 203)),
    );

    let dark_icon = egui::Color32::from_rgb(32, 32, 32);
    if t < 0.5 {
        let weight = (1.0 - t * 2.0).clamp(0.0, 1.0);
        let icon_col = lerp_color(knob_fill, dark_icon, weight);
        draw_sun_icon(painter, knob_center, 5.3, icon_col);
    } else {
        let weight = (t * 2.0 - 1.0).clamp(0.0, 1.0);
        let icon_col = lerp_color(knob_fill, dark_icon, weight);
        draw_moon_icon(painter, knob_center, 5.2, icon_col, knob_fill);
    }

    response
}

fn draw_pill_switch(
    ui: &egui::Ui,
    theme: &ThemeColors,
    rect: egui::Rect,
    id: egui::Id,
    is_on: bool,
    enabled: bool,
    hovered: bool,
) {
    let t = ui.ctx().animate_bool_with_time(id, is_on, 0.16);
    let track_off = egui::Color32::WHITE;
    let track_on = egui::Color32::from_rgb(52, 168, 83); // #34A853
    let border_off = theme.power_border_off;
    let border_on = egui::Color32::from_rgb(44, 146, 72);

    let mut track_fill = lerp_color(track_off, track_on, t);
    let mut track_border = lerp_color(border_off, border_on, t);
    if !enabled {
        track_fill = lerp_color(track_fill, theme.bg_track, 0.45);
        track_border = lerp_color(track_border, theme.border_med, 0.45);
    } else if hovered {
        track_border = lerp_color(track_border, theme.text_muted, 0.25);
    }

    let radius = egui::CornerRadius::same(14);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track_fill);
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.2_f32, track_border),
        egui::StrokeKind::Inside,
    );

    let cx = egui::lerp((rect.left() + 14.0)..=(rect.right() - 14.0), t);
    let cy = rect.center().y;
    painter.circle_filled(
        egui::pos2(cx, cy + 1.2),
        10.8,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 22),
    );
    let knob_fill = if enabled {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(240, 240, 240)
    };
    painter.circle_filled(egui::pos2(cx, cy), 10.3, knob_fill);
    painter.circle_stroke(
        egui::pos2(cx, cy),
        10.3,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(218, 218, 218)),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyRowAction {
    None,
    ToggleRecord,
    Secondary,
}

#[allow(clippy::too_many_arguments)]
fn render_hotkey_row(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    badge_label: Option<String>,
    description: &str,
    is_recording: bool,
    is_busy: bool,
    secondary_label: Option<&str>,
    empty_action_label: &str,
) -> HotkeyRowAction {
    let mut action = HotkeyRowAction::None;
    ui.horizontal(|ui| {
        let has_binding = badge_label.is_some();
        let (fill, text_color, strong, text) = match badge_label {
            Some(lbl) => (theme.bg_keycap, theme.text_primary, true, lbl),
            None => (
                theme.bg_callout,
                theme.text_muted,
                false,
                "No hotkey set".to_string(),
            ),
        };
        egui::Frame::new()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
            .corner_radius(egui::CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                let mut rt = egui::RichText::new(text).size(11.5).color(text_color);
                if strong {
                    rt = rt.strong();
                }
                ui.label(rt);
            });
        ui.label(
            egui::RichText::new(description)
                .size(12.0)
                .color(theme.text_muted),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(sec_lbl) = secondary_label.filter(|_| !is_recording) {
                if ui
                    .add_enabled(
                        !is_busy,
                        egui::Button::new(
                            egui::RichText::new(sec_lbl)
                                .size(11.0)
                                .color(theme.text_muted),
                        )
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::new(1.0_f32, theme.border_subtle))
                        .corner_radius(egui::CornerRadius::same(5))
                        .min_size(egui::vec2(48.0, 22.0)),
                    )
                    .clicked()
                {
                    action = HotkeyRowAction::Secondary;
                }
            }

            let action_label = if is_recording {
                "Press keys... (Esc)"
            } else if has_binding {
                "Change Hotkey"
            } else {
                empty_action_label
            };
            if ui
                .add_enabled(
                    !is_busy,
                    egui::Button::new(
                        egui::RichText::new(action_label)
                            .size(11.0)
                            .color(theme.text_primary),
                    )
                    .fill(if is_recording {
                        theme.bg_keycap
                    } else {
                        theme.btn_sec_bg
                    })
                    .stroke(egui::Stroke::new(1.0_f32, theme.border_med))
                    .corner_radius(egui::CornerRadius::same(5))
                    .min_size(egui::vec2(96.0, 22.0)),
                )
                .clicked()
            {
                action = HotkeyRowAction::ToggleRecord;
            }
        });
    });
    action
}

#[allow(clippy::too_many_arguments)]
fn switch_row_widget(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    pill_id: egui::Id,
    height: f32,
    font_size: f32,
    label: &str,
    is_on: bool,
    enabled: bool,
    show_spinner: bool,
) -> egui::Response {
    let width = ui.available_width();
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let response = if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    };

    let (box_fill, box_border) = if enabled && response.is_pointer_button_down_on() {
        (theme.bg_callout_active, theme.border_med)
    } else if enabled && response.hovered() {
        (theme.bg_callout_hover, theme.border_med)
    } else {
        (theme.bg_callout, theme.border_subtle)
    };

    let box_radius = egui::CornerRadius::same(8);
    ui.painter().rect_filled(rect, box_radius, box_fill);
    ui.painter().rect_stroke(
        rect,
        box_radius,
        egui::Stroke::new(1.0_f32, box_border),
        egui::StrokeKind::Inside,
    );

    let text_left = rect.left() + 12.0 + if show_spinner { 24.0 } else { 0.0 };
    let switch_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 12.0 - 25.0, rect.center().y),
        egui::vec2(50.0, 28.0),
    );

    if show_spinner {
        let spinner_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            egui::vec2(15.0, 15.0),
        );
        ui.put(spinner_rect, egui::Spinner::new().size(14.0));
    }

    let text_color = if enabled {
        theme.text_primary
    } else {
        theme.text_muted
    };
    let avail_text_w = (switch_rect.left() - 10.0 - text_left).max(40.0);
    let galley = egui::WidgetText::from(
        egui::RichText::new(label)
            .size(font_size)
            .strong()
            .color(text_color),
    )
    .into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        avail_text_w,
        egui::TextStyle::Button,
    );
    let text_pos = egui::pos2(text_left, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(text_pos, galley, text_color);

    draw_pill_switch(
        ui,
        theme,
        switch_rect,
        pill_id,
        is_on,
        enabled,
        response.hovered(),
    );

    response
}

fn toggle_switch_row(
    ui: &mut egui::Ui,
    theme: &ThemeColors,
    pill_id: egui::Id,
    label: &str,
    is_on: bool,
    enabled: bool,
) -> egui::Response {
    switch_row_widget(ui, theme, pill_id, 38.0, 12.5, label, is_on, enabled, false)
}

fn apply_theme_visuals(ctx: &egui::Context, dark_mode: bool) {
    let theme = ThemeColors::for_mode(dark_mode);
    let mut v = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.window_fill = theme.bg_window;
    v.panel_fill = theme.bg_window;
    v.extreme_bg_color = theme.bg_input;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, theme.border_subtle);
    v.widgets.inactive.bg_fill = theme.bg_input;
    v.widgets.inactive.weak_bg_fill = theme.bg_input;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, theme.border_med);
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, theme.accent_fill);
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, theme.text_primary);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.2_f32, theme.accent_fill);
    v.selection.bg_fill = theme.selection_bg;
    v.selection.stroke = egui::Stroke::new(1.0_f32, theme.accent_fill);
    ctx.set_visuals(v);

    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.floating = true;
    scroll.floating_allocated_width = 12.0;
    scroll.floating_width = 6.0;
    scroll.bar_width = 8.0;
    scroll.bar_inner_margin = 4.0;
    scroll.bar_outer_margin = 0.0;
    scroll.handle_min_length = 32.0;
    scroll.foreground_color = true;
    scroll.dormant_background_opacity = 0.65;
    scroll.active_background_opacity = 0.85;
    scroll.interact_background_opacity = 1.0;
    scroll.dormant_handle_opacity = 0.35;
    scroll.active_handle_opacity = 0.60;
    scroll.interact_handle_opacity = 0.90;
    ctx.all_styles_mut(|style| {
        style.spacing.scroll = scroll;
    });
}

fn reap_finished(slot: &mut Option<JoinHandle<()>>) -> bool {
    if slot.as_ref().is_some_and(|j| j.is_finished()) {
        if let Some(j) = slot.take() {
            let _ = j.join();
        }
    }
    slot.is_some()
}

fn push_overlay_now(
    overlay: &Option<Arc<OverlayHandle>>,
    shared_overlay_cfg: &Arc<Mutex<OverlayConfig>>,
    state: &Arc<Mutex<SharedState>>,
    overlay_hold: &mut OverlayDisplayHold,
    last_overlay_payload: &mut OverlayPayload,
) -> bool {
    let Some(ov) = overlay.as_ref() else {
        return false;
    };
    let ov_cfg = shared_overlay_cfg.lock().unwrap().clone();
    let shows_tl = ov_cfg
        .lines
        .iter()
        .any(|l| l.visible && l.source == SubtitleLineSource::Translation);
    let (next_payload, pending) = {
        let s = state.lock().unwrap();
        let fin_refs: Vec<FinalizedLine<'_>> = s
            .transcript
            .iter()
            .map(|e| FinalizedLine {
                id: e.id,
                original: &e.original,
                translation: &e.translation,
            })
            .collect();
        overlay_hold.resolve(
            Instant::now(),
            &fin_refs,
            &s.interim_original,
            &s.interim_translation,
            s.interim_confirmed_bytes,
            ov_cfg.live_style,
            shows_tl,
            ov_cfg.max_lines,
            ov_cfg.min_display_sec,
        )
    };
    if next_payload != *last_overlay_payload {
        *last_overlay_payload = next_payload.clone();
        ov.set_payload(next_payload);
    }
    pending
}

#[allow(clippy::too_many_arguments)]
fn controller_loop(
    rx: Receiver<CtrlCmd>,
    tx: Sender<CtrlCmd>,
    state: Arc<Mutex<SharedState>>,
    shared_models_dir: Arc<Mutex<PathBuf>>,
    shared_asr_url: Arc<Mutex<String>>,
    shared_llm_url: Arc<Mutex<String>>,
    shared_audio_vad: Arc<Mutex<AudioVadConfig>>,
    shared_overlay_cfg: Arc<Mutex<OverlayConfig>>,
    shared_show_notifications: Arc<AtomicBool>,
    rms_bits: Arc<AtomicU32>,
    vad_prob_bits: Arc<AtomicU32>,
    mut llm: Arc<LlmHandle>,
    mut hotkey: HotkeyController,
    shell: Option<ShellBridge>,
    overlay: Option<Arc<OverlayHandle>>,
    ctx: egui::Context,
    init_cap_hk: Option<HotkeyBinding>,
    init_ov_hk: Option<HotkeyBinding>,
    init_ct_hk: Option<HotkeyBinding>,
) {
    let mut reg_cap_hk = init_cap_hk;
    if reg_cap_hk.is_some() {
        if let Err(e) = hotkey.set_capture_hotkey(reg_cap_hk) {
            reg_cap_hk = None;
            if let Some(sh) = shell.as_ref() {
                sh.set_hotkey_label(String::new());
                sh.sync_state(false, false);
            }
            let mut s = state.lock().unwrap();
            s.hotkey_revert.push((HotkeyTarget::Capture, None));
            s.error = Some(e.to_string());
        }
    }
    let mut reg_ov_hk = init_ov_hk;
    if reg_ov_hk.is_some() {
        if let Err(e) = hotkey.set_overlay_hotkey(reg_ov_hk) {
            reg_ov_hk = None;
            let mut s = state.lock().unwrap();
            s.hotkey_revert.push((HotkeyTarget::Overlay, None));
            s.error = Some(e.to_string());
        }
    }
    let mut reg_ct_hk = init_ct_hk;
    if reg_ct_hk.is_some() {
        if let Err(e) = hotkey.set_click_through_hotkey(reg_ct_hk) {
            reg_ct_hk = None;
            let mut s = state.lock().unwrap();
            s.hotkey_revert.push((HotkeyTarget::ClickThrough, None));
            s.error = Some(e.to_string());
        }
    }

    let sync_shell = |is_on: bool, is_busy: bool, progress: Option<TaskbarProgress>| {
        if let Some(sh) = shell.as_ref() {
            if let Some(p) = progress {
                sh.set_progress(p);
            }
            sh.sync_state(is_on, is_busy);
        }
        ctx.request_repaint();
    };

    let mut active_audio: Option<AudioCaptureHandle> = None;
    let mut active_asr: Option<AsrWorkerHandle> = None;
    let mut active_coord: Option<TranslationCoordinator> = None;
    let mut active_av_cfg = shared_audio_vad.lock().unwrap().clone();
    let mut active_session_id: u64 = 0;
    let mut overlay_hold = OverlayDisplayHold::default();
    let mut last_overlay_payload = OverlayPayload::default();
    let mut pending_hold_transition = false;
    let mut dl_join: Option<JoinHandle<()>> = None;
    let mut bench_join: Option<JoinHandle<()>> = None;
    let shutdown_cancel = Arc::new(AtomicBool::new(false));

    let stop_pipeline = |audio: &mut Option<AudioCaptureHandle>,
                         asr: &mut Option<AsrWorkerHandle>,
                         coord: &mut Option<TranslationCoordinator>,
                         session_id_slot: &mut u64| {
        *session_id_slot = session_id_slot.wrapping_add(1);
        if let Some(mut a) = audio.take() {
            a.stop();
        }
        if let Some(mut w) = asr.take() {
            w.stop();
        }
        if let Some(mut c) = coord.take() {
            c.stop();
        }
        llm.unload();
        rms_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
        vad_prob_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
    };

    loop {
        let cmd = if pending_hold_transition {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => cmd,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    pending_hold_transition = push_overlay_now(
                        &overlay,
                        &shared_overlay_cfg,
                        &state,
                        &mut overlay_hold,
                        &mut last_overlay_payload,
                    );
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            }
        };

        let dl_active = reap_finished(&mut dl_join);
        let bench_active = reap_finished(&mut bench_join);

        match cmd {
            CtrlCmd::ToggleCapture => {
                let (is_on, is_busy, is_dl, is_bench) = {
                    let s = state.lock().unwrap();
                    (
                        s.capture_phase.is_on(),
                        s.capture_phase.is_busy(),
                        s.download.is_some(),
                        s.benchmarking,
                    )
                };
                if is_busy || is_dl || is_bench || dl_active || bench_active {
                    continue;
                }
                if is_on {
                    let _ = tx.send(CtrlCmd::StopCapture);
                } else {
                    let _ = tx.send(CtrlCmd::StartCapture {
                        models_dir: shared_models_dir.lock().unwrap().clone(),
                        asr_url: shared_asr_url.lock().unwrap().clone(),
                        llm_url: shared_llm_url.lock().unwrap().clone(),
                        audio_vad: shared_audio_vad.lock().unwrap().clone(),
                    });
                }
            }
            CtrlCmd::StartCapture {
                models_dir,
                asr_url,
                llm_url,
                audio_vad,
            } => {
                let can_start = {
                    let s = state.lock().unwrap();
                    matches!(s.capture_phase, CapturePhase::Off)
                        && s.download.is_none()
                        && !s.benchmarking
                        && !dl_active
                        && !bench_active
                };
                if !can_start {
                    continue;
                }

                // Validate models cached
                if !matches!(inspect_asr_cache(&models_dir, &asr_url), CacheStatus::Ready(_)) {
                    state.lock().unwrap().error =
                        Some("ASR model is not downloaded yet. Click Download Model on the ASR card first.".to_string());
                    ctx.request_repaint();
                    continue;
                }
                let Ok(norm_llm) = normalize_hf_url(&llm_url) else {
                    state.lock().unwrap().error = Some("Invalid LLM Hugging Face URL.".to_string());
                    ctx.request_repaint();
                    continue;
                };
                let Some(llm_path) = resolve_existing_llm_path(&models_dir, &norm_llm) else {
                    state.lock().unwrap().error =
                        Some("Translation LLM model is not downloaded yet. Click Download Model on the LLM card first.".to_string());
                    ctx.request_repaint();
                    continue;
                };

                {
                    let mut s = state.lock().unwrap();
                    s.capture_phase = CapturePhase::Loading;
                    s.error = None;
                    s.notice = "Loading Silero VAD, ASR & LLM...".to_string();
                }
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                // Ensure Silero VAD model exists if Silero mode selected
                if audio_vad.vad_mode == VadMode::Silero && !silero_vad_path(&models_dir).is_file() {
                    if let Err(e) =
                        ensure_silero_vad_only(&models_dir, Arc::clone(&shutdown_cancel), |_| {})
                    {
                        let mut s = state.lock().unwrap();
                        s.capture_phase = CapturePhase::Off;
                        s.error = Some(format!("Failed to download Silero VAD: {e}"));
                        s.notice = "Ready".to_string();
                        sync_shell(false, false, Some(TaskbarProgress::None));
                        continue;
                    }
                }

                let load_res = llm.load(llm_path, audio_vad.compute_backend);
                let load_info: LoadInfo = match load_res {
                    Ok(info) => info,
                    Err(e) => {
                        let mut s = state.lock().unwrap();
                        s.capture_phase = CapturePhase::Off;
                        s.error = Some(e.to_string());
                        s.notice = "Ready".to_string();
                        sync_shell(false, false, Some(TaskbarProgress::None));
                        continue;
                    }
                };

                active_session_id = active_session_id.wrapping_add(1);
                let session_id = active_session_id;

                let tx_err = tx.clone();
                let audio_res = AudioCaptureHandle::start(
                    audio_vad.loopback_device_id.clone(),
                    audio_vad.mic_device_id.clone(),
                    Arc::clone(&rms_bits),
                    Arc::new(move |err| {
                        let _ = tx_err.send(CtrlCmd::AudioFatalError {
                            session_id,
                            error: err,
                        });
                    }),
                );
                let (mut audio_handle, audio_rx) = match audio_res {
                    Ok(v) => v,
                    Err(e) => {
                        active_session_id = active_session_id.wrapping_add(1);
                        llm.unload();
                        let mut s = state.lock().unwrap();
                        s.capture_phase = CapturePhase::Off;
                        s.error = Some(e.to_string());
                        s.notice = "Ready".to_string();
                        sync_shell(false, false, Some(TaskbarProgress::None));
                        continue;
                    }
                };

                let tx_asr = tx.clone();
                let ctx_asr = ctx.clone();
                let asr_res = AsrWorkerHandle::start(
                    models_dir.clone(),
                    asr_url.clone(),
                    audio_vad.clone(),
                    audio_rx,
                    Arc::clone(&vad_prob_bits),
                    Arc::new(move |ev| {
                        let _ = tx_asr.send(CtrlCmd::AsrEvent {
                            session_id,
                            event: ev,
                        });
                        ctx_asr.request_repaint();
                    }),
                );
                let asr_handle = match asr_res {
                    Ok(w) => w,
                    Err(e) => {
                        active_session_id = active_session_id.wrapping_add(1);
                        audio_handle.stop();
                        llm.unload();
                        let mut s = state.lock().unwrap();
                        s.capture_phase = CapturePhase::Off;
                        s.error = Some(e.to_string());
                        s.notice = "Ready".to_string();
                        sync_shell(false, false, Some(TaskbarProgress::None));
                        continue;
                    }
                };

                let tx_tl_p = tx.clone();
                let ctx_tl_p = ctx.clone();
                let tx_tl = tx.clone();
                let ctx_tl = ctx.clone();
                let coord = TranslationCoordinator::start(
                    Arc::clone(&llm),
                    Arc::new(move |utterance_id, original, translation, confirmed_bytes| {
                        let _ = tx_tl_p.send(CtrlCmd::TranslationPartial {
                            session_id,
                            utterance_id,
                            original,
                            translation,
                            confirmed_bytes,
                        });
                        ctx_tl_p.request_repaint();
                    }),
                    Arc::new(move |res| {
                        let _ = tx_tl.send(CtrlCmd::TranslationEvent {
                            session_id,
                            result: res,
                        });
                        ctx_tl.request_repaint();
                    }),
                );

                active_audio = Some(audio_handle);
                active_asr = Some(asr_handle);
                active_coord = Some(coord);
                active_av_cfg = audio_vad.clone();

                let asr_name = asr_model_dir(&models_dir, &asr_url)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("ASR")
                    .to_string();

                {
                    let mut s = state.lock().unwrap();
                    s.capture_phase = CapturePhase::Listening {
                        backend_desc: load_info.backend_desc.clone(),
                    };
                    s.active_asr_name = asr_name;
                    s.active_backend_desc = load_info.backend_desc.clone();
                    s.interim_utterance_id = 0;
                    s.last_finalized_utterance_id = 0;
                    s.interim_original.clear();
                    s.interim_translation.clear();
                    s.interim_confirmed_bytes = 0;
                    s.error = None;
                    s.notice = format!(
                        "Listening · {} + {}",
                        s.active_asr_name, load_info.model_name
                    );
                }
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                sync_shell(true, false, Some(TaskbarProgress::None));

                if shared_show_notifications.load(Ordering::Relaxed) {
                    if let Some(sh) = shell.as_ref() {
                        sh.notify(
                            "Translate Live — Listening",
                            format!("{} · {}", load_info.backend_desc, load_info.model_name),
                            true,
                        );
                    }
                }
            }
            CtrlCmd::StopCapture => {
                {
                    let mut s = state.lock().unwrap();
                    if !s.capture_phase.is_on() {
                        continue;
                    }
                    s.capture_phase = CapturePhase::Stopping;
                    s.notice = "Stopping capture & releasing VRAM...".to_string();
                }
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));
                stop_pipeline(
                    &mut active_audio,
                    &mut active_asr,
                    &mut active_coord,
                    &mut active_session_id,
                );
                {
                    let mut s = state.lock().unwrap();
                    s.capture_phase = CapturePhase::Off;
                    s.interim_utterance_id = 0;
                    s.last_finalized_utterance_id = 0;
                    s.interim_original.clear();
                    s.interim_translation.clear();
                    s.interim_confirmed_bytes = 0;
                    s.notice = "Ready · VRAM free".to_string();
                }
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                sync_shell(false, false, Some(TaskbarProgress::None));
                if shared_show_notifications.load(Ordering::Relaxed) {
                    if let Some(sh) = shell.as_ref() {
                        sh.notify("Translate Live — Stopped", "Capture stopped · VRAM free", false);
                    }
                }
            }
            CtrlCmd::AudioFatalError { session_id, error } => {
                if session_id != active_session_id {
                    continue;
                }
                stop_pipeline(
                    &mut active_audio,
                    &mut active_asr,
                    &mut active_coord,
                    &mut active_session_id,
                );
                {
                    let mut s = state.lock().unwrap();
                    s.capture_phase = CapturePhase::Off;
                    s.interim_utterance_id = 0;
                    s.last_finalized_utterance_id = 0;
                    s.interim_original.clear();
                    s.interim_translation.clear();
                    s.interim_confirmed_bytes = 0;
                    s.error = Some(error);
                    s.notice = "Audio capture stopped due to device error".to_string();
                }
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                sync_shell(false, false, Some(TaskbarProgress::None));
            }
            CtrlCmd::UpdateAudioVad(next_av) => {
                if next_av.source_lang != active_av_cfg.source_lang
                    || next_av.target_lang != active_av_cfg.target_lang
                {
                    llm.clear_context();
                }
                active_av_cfg = next_av.clone();
                if let Some(asr) = active_asr.as_ref() {
                    asr.update_config(next_av);
                }
            }
            CtrlCmd::AsrEvent { session_id, event } => {
                if session_id != active_session_id {
                    continue;
                }
                match event {
                    AsrEvent::Interim {
                        utterance_id,
                        text,
                        asr_ms,
                    } => {
                        {
                            let mut s = state.lock().unwrap();
                            if utterance_id <= s.last_finalized_utterance_id {
                                continue;
                            }
                            s.asr_ms = Some(asr_ms);
                            if s.interim_utterance_id != utterance_id {
                                s.interim_utterance_id = utterance_id;
                                s.interim_translation.clear();
                                s.interim_confirmed_bytes = 0;
                            }
                            s.interim_original = text.clone();
                        }
                        pending_hold_transition = push_overlay_now(
                            &overlay,
                            &shared_overlay_cfg,
                            &state,
                            &mut overlay_hold,
                            &mut last_overlay_payload,
                        );
                        let style = shared_overlay_cfg.lock().unwrap().live_style;
                        if style == LiveSubtitleStyle::PrefixLockedFill {
                            if let Some(coord) = active_coord.as_ref() {
                                coord.submit(TranslateRequest {
                                    utterance_id,
                                    text,
                                    source_lang: active_av_cfg.source_lang.clone(),
                                    target_lang: active_av_cfg.target_lang.clone(),
                                    context_pairs: active_av_cfg.context_pairs,
                                    forced_prefix: None,
                                    is_final: false,
                                });
                            }
                        }
                    }
                    AsrEvent::Final {
                        utterance_id,
                        text,
                        asr_ms,
                    } => {
                        {
                            let mut s = state.lock().unwrap();
                            s.asr_ms = Some(asr_ms);
                            if s.interim_utterance_id != utterance_id {
                                s.interim_utterance_id = utterance_id;
                                s.interim_translation.clear();
                                s.interim_confirmed_bytes = 0;
                            }
                            s.interim_original = text.clone();
                        }
                        pending_hold_transition = push_overlay_now(
                            &overlay,
                            &shared_overlay_cfg,
                            &state,
                            &mut overlay_hold,
                            &mut last_overlay_payload,
                        );
                        if let Some(coord) = active_coord.as_ref() {
                            coord.submit(TranslateRequest {
                                utterance_id,
                                text,
                                source_lang: active_av_cfg.source_lang.clone(),
                                target_lang: active_av_cfg.target_lang.clone(),
                                context_pairs: active_av_cfg.context_pairs,
                                forced_prefix: None,
                                is_final: true,
                            });
                        }
                    }
                    AsrEvent::Error(err) => {
                        state.lock().unwrap().error = Some(err);
                    }
                }
            }
            CtrlCmd::TranslationPartial {
                session_id,
                utterance_id,
                original,
                translation,
                confirmed_bytes,
            } => {
                if session_id != active_session_id || translation.trim().is_empty() {
                    continue;
                }
                let updated = {
                    let mut s = state.lock().unwrap();
                    if utterance_id <= s.last_finalized_utterance_id
                        || utterance_id < s.interim_utterance_id
                    {
                        false
                    } else {
                        let same_utt = s.interim_utterance_id == utterance_id;
                        let allow = !same_utt
                            || s.interim_translation.is_empty()
                            || translation.chars().count() >= s.interim_translation.chars().count();
                        if allow {
                            s.interim_confirmed_bytes = if same_utt {
                                s.interim_confirmed_bytes.max(confirmed_bytes)
                            } else {
                                confirmed_bytes
                            }
                            .min(translation.len());
                            s.interim_utterance_id = utterance_id;
                            s.interim_original = original;
                            s.interim_translation = translation;
                            true
                        } else {
                            false
                        }
                    }
                };
                if updated {
                    pending_hold_transition = push_overlay_now(
                        &overlay,
                        &shared_overlay_cfg,
                        &state,
                        &mut overlay_hold,
                        &mut last_overlay_payload,
                    );
                }
            }
            CtrlCmd::TranslationEvent { session_id, result } => {
                if session_id != active_session_id {
                    continue;
                }
                match result {
                    Ok(out) => {
                        let accepted = {
                            let mut s = state.lock().unwrap();
                            if out.elapsed_ms > 0 {
                                s.tl_ms = Some(out.elapsed_ms);
                                s.tok_per_sec = Some(out.tokens_per_sec);
                            }
                            if out.is_final {
                                s.last_finalized_utterance_id =
                                    s.last_finalized_utterance_id.max(out.utterance_id);
                                let entry_id = s.next_transcript_id;
                                s.next_transcript_id = s.next_transcript_id.wrapping_add(1).max(1);
                                s.total_finalized_count = s.total_finalized_count.saturating_add(1);
                                s.transcript_version = s.transcript_version.wrapping_add(1);
                                let ts = Local::now().format("%H:%M:%S").to_string();
                                s.transcript.push(TranscriptEntry {
                                    id: entry_id,
                                    timestamp: ts,
                                    original: out.original,
                                    translation: out.translation,
                                });
                                if s.transcript.len() > 500 {
                                    let excess = s.transcript.len() - 500;
                                    s.transcript.drain(..excess);
                                }
                                if s.interim_utterance_id <= out.utterance_id {
                                    s.interim_original.clear();
                                    s.interim_translation.clear();
                                    s.interim_confirmed_bytes = 0;
                                }
                                true
                            } else if out.utterance_id <= s.last_finalized_utterance_id
                                || out.utterance_id < s.interim_utterance_id
                            {
                                false
                            } else {
                                s.interim_utterance_id = out.utterance_id;
                                s.interim_original = out.original;
                                s.interim_confirmed_bytes =
                                    out.confirmed_prefix_bytes.min(out.translation.len());
                                s.interim_translation = out.translation;
                                true
                            }
                        };
                        if accepted {
                            pending_hold_transition = push_overlay_now(
                                &overlay,
                                &shared_overlay_cfg,
                                &state,
                                &mut overlay_hold,
                                &mut last_overlay_payload,
                            );
                        }
                    }
                    Err(err) => {
                        state.lock().unwrap().error = Some(err.to_string());
                    }
                }
            }
            CtrlCmd::ToggleOverlay => {
                let next_cfg = {
                    let mut cfg = shared_overlay_cfg.lock().unwrap();
                    cfg.visible = !cfg.visible;
                    cfg.clone()
                };
                let is_on = {
                    let mut s = state.lock().unwrap();
                    s.overlay_sync_visible = Some(next_cfg.visible);
                    s.capture_phase.is_on()
                };
                if let Some(ov) = overlay.as_ref() {
                    ov.set_config(next_cfg.clone());
                }
                if let Some(sh) = shell.as_ref() {
                    sh.set_overlay_visible(next_cfg.visible);
                    if shared_show_notifications.load(Ordering::Relaxed) {
                        let body = if next_cfg.visible {
                            "Subtitle overlay shown"
                        } else {
                            "Subtitle overlay hidden"
                        };
                        sh.notify("Translate Live", body, is_on);
                    }
                }
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                ctx.request_repaint();
            }
            CtrlCmd::ToggleClickThrough => {
                let next_cfg = {
                    let mut cfg = shared_overlay_cfg.lock().unwrap();
                    cfg.click_through = !cfg.click_through;
                    cfg.clone()
                };
                let is_on = {
                    let mut s = state.lock().unwrap();
                    s.overlay_sync_click_through = Some(next_cfg.click_through);
                    s.capture_phase.is_on()
                };
                if let Some(ov) = overlay.as_ref() {
                    ov.set_config(next_cfg.clone());
                }
                if shared_show_notifications.load(Ordering::Relaxed) {
                    if let Some(sh) = shell.as_ref() {
                        let body = if next_cfg.click_through {
                            "Overlay click-through enabled"
                        } else {
                            "Overlay click-through disabled"
                        };
                        sh.notify("Translate Live", body, is_on);
                    }
                }
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                ctx.request_repaint();
            }
            CtrlCmd::RefreshOverlay => {
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
            }
            CtrlCmd::ClearTranscript => {
                {
                    let mut s = state.lock().unwrap();
                    s.transcript.clear();
                    s.interim_original.clear();
                    s.interim_translation.clear();
                    s.interim_confirmed_bytes = 0;
                    s.total_finalized_count = 0;
                    s.transcript_version = s.transcript_version.wrapping_add(1);
                }
                llm.clear_context();
                pending_hold_transition = push_overlay_now(
                    &overlay,
                    &shared_overlay_cfg,
                    &state,
                    &mut overlay_hold,
                    &mut last_overlay_payload,
                );
                ctx.request_repaint();
            }
            CtrlCmd::DownloadModel {
                kind,
                url,
                models_dir,
            } => {
                let busy = {
                    let s = state.lock().unwrap();
                    s.download.is_some()
                        || s.benchmarking
                        || s.capture_phase.is_on()
                        || s.capture_phase.is_busy()
                        || dl_active
                        || bench_active
                };
                if busy {
                    continue;
                }
                let cancel = Arc::new(AtomicBool::new(false));
                {
                    let mut s = state.lock().unwrap();
                    s.error = None;
                    s.download = Some(DownloadState {
                        kind,
                        downloaded: 0,
                        total: None,
                        speed_bps: 0.0,
                        label: match kind {
                            ModelKind::Asr => display_asr_name(&url),
                            ModelKind::Llm => normalize_hf_url(&url)
                                .map(|u| display_llm_name(&u))
                                .unwrap_or_else(|_| "model.gguf".to_string()),
                        },
                        cancel: Arc::clone(&cancel),
                    });
                }
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                let state_dl = Arc::clone(&state);
                let shell_dl = shell.clone();
                let ctx_dl = ctx.clone();
                let cancel_dl = Arc::clone(&cancel);

                match thread::Builder::new()
                    .name("model-downloader".to_string())
                    .spawn(move || {
                        let emit_prog = |p: DownloadProgress| {
                            {
                                let mut s = state_dl.lock().unwrap();
                                if let Some(d) = s.download.as_mut() {
                                    d.downloaded = p.downloaded;
                                    d.total = p.total;
                                    d.speed_bps = p.speed_bps;
                                    d.label = p.label.clone();
                                }
                            }
                            if let Some(sh) = shell_dl.as_ref() {
                                let tb = match p.total {
                                    Some(tot) if tot > 0 => TaskbarProgress::Determinate {
                                        current: p.downloaded,
                                        total: tot,
                                    },
                                    _ => TaskbarProgress::Indeterminate,
                                };
                                sh.set_progress(tb);
                            }
                            ctx_dl.request_repaint();
                        };

                        let res = match kind {
                            ModelKind::Asr => download_asr_model(
                                &url,
                                &models_dir,
                                true,
                                cancel_dl,
                                emit_prog,
                            )
                            .map(|_| ()),
                            ModelKind::Llm => {
                                download_llm_model(&url, &models_dir, cancel_dl, emit_prog)
                                    .map(|_| ())
                            }
                        };

                        {
                            let mut s = state_dl.lock().unwrap();
                            s.download = None;
                            s.cache_refresh_needed = true;
                            match res {
                                Ok(()) => {
                                    s.error = None;
                                    s.notice = "Model downloaded and ready".to_string();
                                }
                                Err(err) => {
                                    let msg = err.to_string();
                                    if msg.contains("cancelled") {
                                        s.notice = "Download paused".to_string();
                                    } else {
                                        s.error = Some(msg);
                                    }
                                }
                            }
                        }
                        if let Some(sh) = shell_dl.as_ref() {
                            sh.set_progress(TaskbarProgress::None);
                            sh.sync_state(false, false);
                        }
                        ctx_dl.request_repaint();
                    })
                {
                    Ok(j) => dl_join = Some(j),
                    Err(e) => {
                        {
                            let mut s = state.lock().unwrap();
                            s.download = None;
                            s.error = Some(format!("Failed to start downloader thread: {e}"));
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::CancelDownload => {
                let cancel = state
                    .lock()
                    .unwrap()
                    .download
                    .as_ref()
                    .map(|d| Arc::clone(&d.cancel));
                if let Some(c) = cancel {
                    c.store(true, Ordering::Relaxed);
                }
            }
            CtrlCmd::RunBenchmark {
                models_dir,
                asr_url,
                llm_url,
                audio_vad,
            } => {
                {
                    let mut s = state.lock().unwrap();
                    if s.benchmarking
                        || s.download.is_some()
                        || s.capture_phase.is_on()
                        || s.capture_phase.is_busy()
                        || dl_active
                        || bench_active
                    {
                        continue;
                    }
                    s.benchmarking = true;
                    s.error = None;
                    s.notice = "Running synthetic ASR & LLM benchmark...".to_string();
                }
                sync_shell(false, true, Some(TaskbarProgress::Indeterminate));

                let state_b = Arc::clone(&state);
                let llm_b = Arc::clone(&llm);
                let shell_b = shell.clone();
                let ctx_b = ctx.clone();

                match thread::Builder::new()
                    .name("bench-runner".to_string())
                    .spawn(move || {
                        let asr_ready =
                            matches!(inspect_asr_cache(&models_dir, &asr_url), CacheStatus::Ready(_));
                        let llm_path = normalize_hf_url(&llm_url)
                            .ok()
                            .and_then(|u| resolve_existing_llm_path(&models_dir, &u));

                        if !asr_ready && llm_path.is_none() {
                            let mut s = state_b.lock().unwrap();
                            s.benchmarking = false;
                            s.error = Some(
                                "Download at least one ASR or LLM model before running Benchmark."
                                    .to_string(),
                            );
                            s.notice = "Ready".to_string();
                            if let Some(sh) = shell_b.as_ref() {
                                sh.set_progress(TaskbarProgress::None);
                                sh.sync_state(false, false);
                            }
                            ctx_b.request_repaint();
                            return;
                        }

                        let mut parts = Vec::new();
                        if asr_ready {
                            match run_asr_benchmark(
                                &models_dir,
                                &asr_url,
                                &audio_vad.source_lang,
                                audio_vad.asr_threads,
                            ) {
                                Ok((name, ms)) => {
                                    state_b.lock().unwrap().asr_ms = Some(ms);
                                    parts.push(format!("ASR ({name}): {ms} ms"));
                                }
                                Err(e) => {
                                    state_b.lock().unwrap().error = Some(e.to_string());
                                }
                            }
                        }

                        if let Some(path) = llm_path {
                            match llm_b.load(path, audio_vad.compute_backend) {
                                Ok(info) => {
                                    let sample = match audio_vad.source_lang.as_str() {
                                        "ko" => "오늘 방송 봐주셔서 정말 감사합니다!",
                                        "zh" | "yue" => "感谢大家今天来看直播！",
                                        _ => "今日の配信を見てくれて本当にありがとう！",
                                    };
                                    match llm_b.translate(TranslateRequest {
                                        utterance_id: 0,
                                        text: sample.to_string(),
                                        source_lang: audio_vad.source_lang.clone(),
                                        target_lang: audio_vad.target_lang.clone(),
                                        context_pairs: 0,
                                        forced_prefix: None,
                                        is_final: false,
                                    }) {
                                        Ok(out) => {
                                            let mut s = state_b.lock().unwrap();
                                            s.tl_ms = Some(out.elapsed_ms);
                                            s.tok_per_sec = Some(out.tokens_per_sec);
                                            s.active_backend_desc = info.backend_desc.clone();
                                            parts.push(format!(
                                                "LLM ({}): {} ms ({:.1} Tok/s)",
                                                info.backend_desc, out.elapsed_ms, out.tokens_per_sec
                                            ));
                                        }
                                        Err(e) => {
                                            state_b.lock().unwrap().error = Some(e.to_string());
                                        }
                                    }
                                    llm_b.unload();
                                }
                                Err(e) => {
                                    state_b.lock().unwrap().error = Some(e.to_string());
                                }
                            }
                        }

                        {
                            let mut s = state_b.lock().unwrap();
                            s.benchmarking = false;
                            if !parts.is_empty() {
                                s.notice = format!("Benchmark · {}", parts.join("  |  "));
                            } else {
                                s.notice = "Ready".to_string();
                            }
                        }
                        if let Some(sh) = shell_b.as_ref() {
                            sh.set_progress(TaskbarProgress::None);
                            sh.sync_state(false, false);
                        }
                        ctx_b.request_repaint();
                    })
                {
                    Ok(j) => bench_join = Some(j),
                    Err(e) => {
                        {
                            let mut s = state.lock().unwrap();
                            s.benchmarking = false;
                            s.error = Some(format!("Failed to start benchmark thread: {e}"));
                        }
                        sync_shell(false, false, Some(TaskbarProgress::None));
                    }
                }
            }
            CtrlCmd::UpdateHotkey(target, next) => {
                let res = match target {
                    HotkeyTarget::Capture => hotkey.set_capture_hotkey(next),
                    HotkeyTarget::Overlay => hotkey.set_overlay_hotkey(next),
                    HotkeyTarget::ClickThrough => hotkey.set_click_through_hotkey(next),
                };
                match res {
                    Ok(()) => {
                        match target {
                            HotkeyTarget::Capture => {
                                reg_cap_hk = next;
                                if let Some(sh) = shell.as_ref() {
                                    sh.set_hotkey_label(
                                        next.map(|b| b.display_label()).unwrap_or_default(),
                                    );
                                    let (is_on, is_busy) = {
                                        let s = state.lock().unwrap();
                                        (
                                            s.capture_phase.is_on(),
                                            s.capture_phase.is_busy()
                                                || s.download.is_some()
                                                || s.benchmarking,
                                        )
                                    };
                                    sh.sync_state(is_on, is_busy);
                                }
                            }
                            HotkeyTarget::Overlay => reg_ov_hk = next,
                            HotkeyTarget::ClickThrough => reg_ct_hk = next,
                        }
                        state.lock().unwrap().error = None;
                    }
                    Err(e) => {
                        let prev = match target {
                            HotkeyTarget::Capture => {
                                let _ = hotkey.set_capture_hotkey(reg_cap_hk);
                                reg_cap_hk
                            }
                            HotkeyTarget::Overlay => {
                                let _ = hotkey.set_overlay_hotkey(reg_ov_hk);
                                reg_ov_hk
                            }
                            HotkeyTarget::ClickThrough => {
                                let _ = hotkey.set_click_through_hotkey(reg_ct_hk);
                                reg_ct_hk
                            }
                        };
                        let mut s = state.lock().unwrap();
                        s.hotkey_revert.push((target, prev));
                        s.error = Some(e.to_string());
                    }
                }
                ctx.request_repaint();
            }
            CtrlCmd::HotkeyEvent(ev) => match ev {
                HotkeyEvent::ToggleCapture => {
                    let _ = tx.send(CtrlCmd::ToggleCapture);
                }
                HotkeyEvent::ToggleOverlay => {
                    let _ = tx.send(CtrlCmd::ToggleOverlay);
                }
                HotkeyEvent::ToggleClickThrough => {
                    let _ = tx.send(CtrlCmd::ToggleClickThrough);
                }
            },
            CtrlCmd::ShowWindow => {
                if let Some(sh) = shell.as_ref() {
                    sh.show_window();
                }
            }
            CtrlCmd::Quit => {
                shutdown_cancel.store(true, Ordering::Relaxed);
                {
                    let mut s = state.lock().unwrap();
                    s.force_quit = true;
                    if let Some(dl) = s.download.as_ref() {
                        dl.cancel.store(true, Ordering::Relaxed);
                    }
                }
                stop_pipeline(
                    &mut active_audio,
                    &mut active_asr,
                    &mut active_coord,
                    &mut active_session_id,
                );
                if let Some(sh) = shell.as_ref() {
                    sh.set_progress(TaskbarProgress::None);
                    sh.request_close_window();
                }
                break;
            }
            CtrlCmd::ShutdownController => {
                shutdown_cancel.store(true, Ordering::Relaxed);
                {
                    let mut s = state.lock().unwrap();
                    s.force_quit = true;
                    if let Some(dl) = s.download.as_ref() {
                        dl.cancel.store(true, Ordering::Relaxed);
                    }
                }
                stop_pipeline(
                    &mut active_audio,
                    &mut active_asr,
                    &mut active_coord,
                    &mut active_session_id,
                );
                break;
            }
        }
    }

    if let Some(j) = dl_join.take() {
        let _ = j.join();
    }
    if let Some(j) = bench_join.take() {
        let _ = j.join();
    }
    hotkey.shutdown();
    if let Some(llm_mut) = Arc::get_mut(&mut llm) {
        llm_mut.shutdown();
    }
}
