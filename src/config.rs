use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_ASR_URL: &str =
    "https://huggingface.co/oboroge0/Haas-ja-v1.0-onnx-quantized";
pub const DEFAULT_LLM_URL: &str =
    "https://huggingface.co/HauhauCS/Qwen3.5-9B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-9B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf";
pub const SILERO_VAD_URL: &str =
    "https://huggingface.co/csukuangfj/vad/resolve/main/silero_vad.onnx";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrArch {
    NemoCtc,
    NemoTransducer { feature_dim: i32 },
    ZipformerTransducer { feature_dim: i32 },
    Paraformer,
    FireRedAsrCtc,
    SenseVoice,
}

#[derive(Debug, Clone, Copy)]
pub struct AsrPreset {
    pub name: &'static str,
    pub url: &'static str,
    pub arch: AsrArch,
    pub slug: &'static str,
    pub files: &'static [&'static str],
    pub subtitle: &'static str,
}

pub const ASR_PRESETS: &[AsrPreset] = &[
    AsrPreset {
        name: "Haas JA v1.0 (Quantized)",
        url: DEFAULT_ASR_URL,
        arch: AsrArch::NemoTransducer { feature_dim: 128 },
        slug: "haas-ja-v1.0-onnx-quantized",
        files: &[
            "encoder.quantized.onnx",
            "encoder.data",
            "decoder.onnx",
            "joiner.onnx",
            "tokens.txt",
        ],
        subtitle: "Default · Japanese FastConformer + KDA TDT (~481 MB)",
    },
    AsrPreset {
        name: "Parakeet TDT-CTC 0.6B (JA)",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8",
        arch: AsrArch::NemoCtc,
        slug: "parakeet-tdt_ctc-0.6b-ja-int8",
        files: &["model.int8.onnx", "tokens.txt"],
        subtitle: "Fast Japanese NeMo CTC (~625 MB)",
    },
    AsrPreset {
        name: "ReazonSpeech k2 v2 (Zipformer JA)",
        url: "https://huggingface.co/reazon-research/reazonspeech-k2-v2",
        arch: AsrArch::ZipformerTransducer { feature_dim: 80 },
        slug: "reazonspeech-k2-v2-int8",
        files: &[
            "encoder-epoch-99-avg-1.int8.onnx",
            "decoder-epoch-99-avg-1.int8.onnx",
            "joiner-epoch-99-avg-1.int8.onnx",
            "tokens.txt",
        ],
        subtitle: "Japanese Zipformer RNN-T (~153 MB)",
    },
    AsrPreset {
        name: "Paraformer Large (ZH / EN)",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09",
        arch: AsrArch::Paraformer,
        slug: "paraformer-zh-2024-03-09-int8",
        files: &["model.int8.onnx", "tokens.txt"],
        subtitle: "Chinese Mandarin + English · Fast Paraformer (~227 MB)",
    },
    AsrPreset {
        name: "FireRedASR2 CTC (ZH / EN)",
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25",
        arch: AsrArch::FireRedAsrCtc,
        slug: "fire-red-asr2-ctc-zh-en-int8",
        files: &["model.int8.onnx", "tokens.txt"],
        subtitle: "Top Chinese + English + Dialects CTC (~776 MB)",
    },
    AsrPreset {
        name: "Zipformer Korean (KO)",
        url: "https://huggingface.co/k2-fsa/sherpa-onnx-zipformer-korean-2024-06-24",
        arch: AsrArch::ZipformerTransducer { feature_dim: 80 },
        slug: "zipformer-korean-2024-06-24-int8",
        files: &[
            "encoder-epoch-99-avg-1.int8.onnx",
            "decoder-epoch-99-avg-1.onnx",
            "joiner-epoch-99-avg-1.int8.onnx",
            "tokens.txt",
        ],
        subtitle: "Korean KsponSpeech Zipformer RNN-T (~85 MB)",
    },
    AsrPreset {
        name: "Parakeet TDT 0.6B v2 (EN)",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        arch: AsrArch::NemoTransducer { feature_dim: 128 },
        slug: "parakeet-tdt-0.6b-v2-en-int8",
        files: &[
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ],
        subtitle: "Best English NVIDIA Parakeet TDT (~661 MB)",
    },
    AsrPreset {
        name: "Parakeet TDT 0.6B v3 (EN + 25 EU)",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        arch: AsrArch::NemoTransducer { feature_dim: 128 },
        slug: "parakeet-tdt-0.6b-v3-eu-int8",
        files: &[
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ],
        subtitle: "English + 25 European Languages TDT (~670 MB)",
    },
    AsrPreset {
        name: "SenseVoice Small (JA / KO / ZH / YUE / EN)",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
        arch: AsrArch::SenseVoice,
        slug: "sense-voice-small-int8",
        files: &["model.int8.onnx", "tokens.txt"],
        subtitle: "Fast Multilingual JA / KO / ZH / YUE / EN (~228 MB)",
    },
];

#[derive(Debug, Clone, Copy)]
pub struct LlmPreset {
    pub name: &'static str,
    pub url: &'static str,
    pub subtitle: &'static str,
}

pub const LLM_PRESETS: &[LlmPreset] = &[
    LlmPreset {
        name: "Qwen3.5-9B Uncensored Q4_K_M (Default)",
        url: DEFAULT_LLM_URL,
        subtitle: "Best uncensored slang, VTubers & rolling context (~5.24 GB)",
    },
    LlmPreset {
        name: "HY-MT1.5-7B Q4_K_M",
        url: "https://huggingface.co/tencent/HY-MT1.5-7B-GGUF/resolve/main/HY-MT1.5-7B-Q4_K_M.gguf",
        subtitle: "Best dedicated multilingual translator · 33+ langs (~4.31 GB)",
    },
    LlmPreset {
        name: "Shisa v2.1 Qwen3-8B Q4_K_M (JA ↔ EN)",
        url: "https://huggingface.co/mradermacher/shisa-v2.1-qwen3-8b-GGUF/resolve/main/shisa-v2.1-qwen3-8b.Q4_K_M.gguf",
        subtitle: "Natural colloquial Japanese ↔ English dialogue (~4.68 GB)",
    },
    LlmPreset {
        name: "TranslateGemma 12B Q4_K_M",
        url: "https://huggingface.co/mradermacher/translategemma-12b-it-GGUF/resolve/main/translategemma-12b-it.Q4_K_M.gguf",
        subtitle: "Google Gemma 3 dedicated 55-language MT (~6.80 GB)",
    },
    LlmPreset {
        name: "Qwen3.5-4B Uncensored Q4_K_M (Fast)",
        url: "https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf",
        subtitle: "Fast low-VRAM uncensored translation (~2.52 GB)",
    },
    LlmPreset {
        name: "HY-MT1.5-1.8B Q8_0 (Ultra-Fast / CPU)",
        url: "https://huggingface.co/tencent/HY-MT1.5-1.8B-GGUF/resolve/main/HY-MT1.5-1.8B-Q8_0.gguf",
        subtitle: "Smallest dedicated MT for low VRAM or CPU (~1.78 GB)",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u32,
}

impl HotkeyBinding {
    pub fn function_key(n: u32) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            vk: 0x6F + n, // F1 = 0x70
        }
    }

    pub fn is_valid(&self) -> bool {
        if vk_to_name(self.vk).is_none() {
            return false;
        }
        let is_function_key = (0x70..=0x7B).contains(&self.vk);
        is_function_key || self.ctrl || self.alt || self.shift || self.win
    }

    pub fn display_label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        let key_name = vk_to_name(self.vk).unwrap_or_else(|| format!("0x{:02X}", self.vk));
        parts.push(key_name);
        parts.join(" + ")
    }
}

pub fn vk_to_name(vk: u32) -> Option<String> {
    match vk {
        0x41..=0x5A => Some(((vk as u8) as char).to_string()),
        0x30..=0x39 => Some(((vk as u8) as char).to_string()),
        0x70..=0x7B => Some(format!("F{}", vk - 0x70 + 1)),
        0x20 => Some("Space".to_string()),
        0x09 => Some("Tab".to_string()),
        0x2D => Some("Insert".to_string()),
        0x2E => Some("Delete".to_string()),
        0x24 => Some("Home".to_string()),
        0x23 => Some("End".to_string()),
        0x21 => Some("PageUp".to_string()),
        0x22 => Some("PageDown".to_string()),
        0x25 => Some("Left".to_string()),
        0x26 => Some("Up".to_string()),
        0x27 => Some("Right".to_string()),
        0x28 => Some("Down".to_string()),
        0xBD => Some("-".to_string()),
        0xBB => Some("=".to_string()),
        0xDB => Some("[".to_string()),
        0xDD => Some("]".to_string()),
        0xBA => Some(";".to_string()),
        0xDE => Some("'".to_string()),
        0xBC => Some(",".to_string()),
        0xBE => Some(".".to_string()),
        0xBF => Some("/".to_string()),
        0xC0 => Some("`".to_string()),
        0xDC => Some("\\".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComputeBackend {
    VulkanGpu,
    CpuOnly,
}

impl ComputeBackend {
    pub fn label(self) -> &'static str {
        match self {
            ComputeBackend::VulkanGpu => "Vulkan GPU (Auto CPU fallback)",
            ComputeBackend::CpuOnly => "CPU Only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VadMode {
    Silero,
    Energy,
}

impl VadMode {
    pub fn label(self) -> &'static str {
        match self {
            VadMode::Silero => "Silero VAD (Local ONNX)",
            VadMode::Energy => "Energy-based",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SilenceMode {
    Fixed,
    Adaptive,
}

impl SilenceMode {
    pub fn label(self) -> &'static str {
        match self {
            SilenceMode::Fixed => "Fixed",
            SilenceMode::Adaptive => "Adaptive",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioVadConfig {
    pub loopback_device_id: Option<String>,
    pub mic_device_id: Option<String>,
    pub compute_backend: ComputeBackend,
    pub asr_threads: i32,
    pub source_lang: String,
    pub target_lang: String,
    pub context_pairs: usize,
    pub vad_mode: VadMode,
    pub silero_threshold_pct: f32,
    pub energy_threshold_pct: f32,
    pub min_speech_sec: f32,
    pub max_speech_sec: f32,
    pub silence_mode: SilenceMode,
    pub silence_dur_sec: f32,
    pub incremental_asr: bool,
    pub interim_interval_sec: f32,
}

impl Default for AudioVadConfig {
    fn default() -> Self {
        let default_threads = std::thread::available_parallelism()
            .map(|n| (n.get() / 2).clamp(2, 6) as i32)
            .unwrap_or(4);
        Self {
            loopback_device_id: None,
            mic_device_id: None,
            compute_backend: ComputeBackend::VulkanGpu,
            asr_threads: default_threads,
            source_lang: "ja".to_string(),
            target_lang: "en".to_string(),
            context_pairs: 5,
            vad_mode: VadMode::Silero,
            silero_threshold_pct: 20.0,
            energy_threshold_pct: 1.5,
            min_speech_sec: 0.25,
            max_speech_sec: 8.0,
            silence_mode: SilenceMode::Adaptive,
            silence_dur_sec: 0.85,
            incremental_asr: true,
            interim_interval_sec: 0.65,
        }
    }
}

impl AudioVadConfig {
    pub fn apply_general_video_profile(&mut self) {
        self.vad_mode = VadMode::Silero;
        self.silero_threshold_pct = 42.0;
        self.energy_threshold_pct = 1.5;
        self.min_speech_sec = 0.25;
        self.max_speech_sec = 8.0;
        self.silence_mode = SilenceMode::Adaptive;
        self.silence_dur_sec = 0.85;
        self.context_pairs = 6;
        self.incremental_asr = true;
        self.interim_interval_sec = 0.65;
    }

    pub fn apply_gaming_stream_profile(&mut self) {
        self.vad_mode = VadMode::Silero;
        self.silero_threshold_pct = 62.0;
        self.energy_threshold_pct = 2.5;
        self.min_speech_sec = 0.30;
        self.max_speech_sec = 6.5;
        self.silence_mode = SilenceMode::Adaptive;
        self.silence_dur_sec = 0.70;
        self.context_pairs = 4;
        self.incremental_asr = true;
        self.interim_interval_sec = 0.65;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleLineSource {
    Original,
    Translation,
}

impl SubtitleLineSource {
    pub fn label(self) -> &'static str {
        match self {
            SubtitleLineSource::Original => "Original",
            SubtitleLineSource::Translation => "Translation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

impl TextAlign {
    pub fn label(self) -> &'static str {
        match self {
            TextAlign::Left => "Left",
            TextAlign::Center => "Center",
            TextAlign::Right => "Right",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleLineConfig {
    pub id: u64,
    pub label: String,
    pub source: SubtitleLineSource,
    pub visible: bool,
    pub font_family: String,
    pub font_size: f32,
    pub color_hex: String,
    pub align: TextAlign,
    pub outline: bool,
}

impl Default for SubtitleLineConfig {
    fn default() -> Self {
        Self {
            id: 1,
            label: "Translation".to_string(),
            source: SubtitleLineSource::Translation,
            visible: true,
            font_family: "Segoe UI".to_string(),
            font_size: 22.0,
            color_hex: "#FFFFFF".to_string(),
            align: TextAlign::Center,
            outline: true,
        }
    }
}

pub fn default_subtitle_lines() -> Vec<SubtitleLineConfig> {
    vec![
        SubtitleLineConfig {
            id: 1,
            label: "Original".to_string(),
            source: SubtitleLineSource::Original,
            visible: false,
            font_family: "Yu Gothic UI".to_string(),
            font_size: 16.0,
            color_hex: "#D8D5CF".to_string(),
            align: TextAlign::Center,
            outline: true,
        },
        SubtitleLineConfig {
            id: 2,
            label: "Translation".to_string(),
            source: SubtitleLineSource::Translation,
            visible: true,
            font_family: "Segoe UI".to_string(),
            font_size: 22.0,
            color_hex: "#FFFFFF".to_string(),
            align: TextAlign::Center,
            outline: true,
        },
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HideAnimation {
    Fade,
    None,
}

impl HideAnimation {
    pub fn label(self) -> &'static str {
        match self {
            HideAnimation::Fade => "Fade",
            HideAnimation::None => "Instant",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveSubtitleStyle {
    PrefixLockedFill,
    CleanSentencesOnly,
}

impl LiveSubtitleStyle {
    pub fn label(self) -> &'static str {
        match self {
            LiveSubtitleStyle::PrefixLockedFill => "Smooth Live Fill (Prefix-Locked)",
            LiveSubtitleStyle::CleanSentencesOnly => "Clean Sentences Only (Zero Flicker)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub live_style: LiveSubtitleStyle,
    pub max_lines: usize,
    pub smooth_scroll: bool,
    pub min_display_sec: f32,
    pub visible: bool,
    pub click_through: bool,
    pub top_most: bool,
    pub bg_opacity_pct: f32,
    pub bg_color_hex: String,
    pub border_radius: f32,
    pub bg_image_path: Option<String>,
    pub auto_hide_sec: f32,
    pub hide_animation: HideAnimation,
    pub hide_duration_ms: u32,
    pub lines: Vec<SubtitleLineConfig>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            live_style: LiveSubtitleStyle::PrefixLockedFill,
            max_lines: 3,
            smooth_scroll: true,
            min_display_sec: 0.75,
            visible: true,
            click_through: false,
            top_most: true,
            bg_opacity_pct: 72.0,
            bg_color_hex: "#141414".to_string(),
            border_radius: 12.0,
            bg_image_path: None,
            auto_hide_sec: 0.0,
            hide_animation: HideAnimation::Fade,
            hide_duration_ms: 300,
            lines: default_subtitle_lines(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptConfig {
    pub visible: bool,
    pub auto_scroll: bool,
    pub container_color_hex: String,
    pub header_color_hex: String,
    pub container_opacity_pct: f32,
    pub font_size: f32,
}

impl Default for TranscriptConfig {
    fn default() -> Self {
        Self {
            visible: true,
            auto_scroll: true,
            container_color_hex: "#181818".to_string(),
            header_color_hex: "#222222".to_string(),
            container_opacity_pct: 100.0,
            font_size: 13.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub asr_model_url: String,
    pub llm_model_url: String,
    pub model_dir: Option<String>,
    pub capture_hotkey: Option<HotkeyBinding>,
    pub overlay_hotkey: Option<HotkeyBinding>,
    pub click_through_hotkey: Option<HotkeyBinding>,
    pub minimize_to_tray_on_close: bool,
    pub show_notifications: bool,
    pub dark_mode: bool,
    pub audio_vad: AudioVadConfig,
    pub overlay: OverlayConfig,
    pub transcript: TranscriptConfig,
    pub main_window: Option<WindowState>,
    pub overlay_window: Option<WindowState>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            asr_model_url: DEFAULT_ASR_URL.to_string(),
            llm_model_url: DEFAULT_LLM_URL.to_string(),
            model_dir: None,
            capture_hotkey: Some(HotkeyBinding::function_key(8)),
            overlay_hotkey: Some(HotkeyBinding::function_key(9)),
            click_through_hotkey: Some(HotkeyBinding::function_key(10)),
            minimize_to_tray_on_close: false,
            show_notifications: true,
            dark_mode: true,
            audio_vad: AudioVadConfig::default(),
            overlay: OverlayConfig::default(),
            transcript: TranscriptConfig::default(),
            main_window: None,
            overlay_window: None,
        }
    }
}

pub const SOURCE_LANGUAGES: &[(&str, &str)] = &[
    ("ja", "ja - 日本語 (Japanese)"),
    ("ko", "ko - 한국어 (Korean)"),
    ("zh", "zh - 中文 (Chinese)"),
    ("yue", "yue - 廣東話 (Cantonese)"),
    ("en", "en - English"),
    ("auto", "Auto (Multilingual)"),
];

pub const TARGET_LANGUAGES: &[(&str, &str)] = &[
    ("en", "en - English"),
    ("ja", "ja - 日本語"),
    ("ko", "ko - 한국어"),
    ("zh", "zh - 中文"),
    ("es", "es - Español"),
    ("fr", "fr - Français"),
    ("de", "de - Deutsch"),
    ("vi", "vi - Tiếng Việt"),
    ("pt", "pt - Português"),
];

pub fn language_prompt_name(code: &str) -> &'static str {
    match code {
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "yue" => "Cantonese",
        "en" => "English",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "vi" => "Vietnamese",
        "pt" => "Portuguese",
        _ => "the source language",
    }
}

pub fn language_chinese_name(code: &str) -> &'static str {
    match code {
        "ja" => "日语",
        "ko" => "韩语",
        "zh" => "中文",
        "yue" => "粤语",
        "en" => "英语",
        "es" => "西班牙语",
        "fr" => "法语",
        "de" => "德语",
        "vi" => "越南语",
        "pt" => "葡萄牙语",
        _ => "英语",
    }
}

pub fn config_file_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let trimmed = appdata.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed)
                .join("Translate-Live")
                .join("settings.json");
        }
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "Translate-Live") {
        dirs.config_dir().join("settings.json")
    } else {
        PathBuf::from("settings.json")
    }
}

/// Default model storage folder: `<exe_dir>\models`.
/// Never creates this directory automatically on startup.
pub fn default_model_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("models")))
        .unwrap_or_else(|| PathBuf::from("models"))
}

pub fn effective_model_dir(cfg: &AppConfig) -> PathBuf {
    if let Some(dir) = cfg.model_dir.as_deref() {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    default_model_dir()
}

pub fn sanitize_hotkey_option(hk: Option<HotkeyBinding>, used: &[HotkeyBinding]) -> Option<HotkeyBinding> {
    let b = hk?;
    if !b.is_valid() || used.contains(&b) {
        None
    } else {
        Some(b)
    }
}

pub fn load_config() -> AppConfig {
    let path = config_file_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return AppConfig::default();
    };
    let mut cfg: AppConfig = serde_json::from_str(&raw).unwrap_or_default();
    if cfg.asr_model_url.trim().is_empty() {
        cfg.asr_model_url = DEFAULT_ASR_URL.to_string();
    }
    if cfg.llm_model_url.trim().is_empty() {
        cfg.llm_model_url = DEFAULT_LLM_URL.to_string();
    }
    if cfg.overlay.lines.is_empty() {
        cfg.overlay.lines = default_subtitle_lines();
    }
    cfg.audio_vad.asr_threads = cfg.audio_vad.asr_threads.clamp(1, 16);
    cfg.audio_vad.context_pairs = cfg.audio_vad.context_pairs.clamp(0, 5);
    cfg.audio_vad.silero_threshold_pct = cfg.audio_vad.silero_threshold_pct.clamp(5.0, 95.0);
    cfg.audio_vad.energy_threshold_pct = cfg.audio_vad.energy_threshold_pct.clamp(0.0, 50.0);
    cfg.audio_vad.min_speech_sec = cfg.audio_vad.min_speech_sec.clamp(0.10, 5.0);
    cfg.audio_vad.max_speech_sec = cfg
        .audio_vad
        .max_speech_sec
        .clamp(cfg.audio_vad.min_speech_sec + 0.5, 30.0);
    cfg.audio_vad.silence_dur_sec = cfg.audio_vad.silence_dur_sec.clamp(0.10, 5.0);
    cfg.audio_vad.interim_interval_sec = cfg.audio_vad.interim_interval_sec.clamp(0.25, 5.0);
    cfg.overlay.bg_opacity_pct = cfg.overlay.bg_opacity_pct.clamp(0.0, 100.0);
    cfg.overlay.border_radius = cfg.overlay.border_radius.clamp(0.0, 36.0);
    cfg.overlay.max_lines = cfg.overlay.max_lines.clamp(1, 4);
    cfg.overlay.min_display_sec = cfg.overlay.min_display_sec.clamp(0.0, 5.0);
    cfg.overlay.auto_hide_sec = cfg.overlay.auto_hide_sec.clamp(0.0, 120.0);
    cfg.overlay.hide_duration_ms = cfg.overlay.hide_duration_ms.clamp(50, 2000);
    cfg.transcript.container_opacity_pct = cfg.transcript.container_opacity_pct.clamp(10.0, 100.0);
    cfg.transcript.font_size = cfg.transcript.font_size.clamp(10.0, 28.0);

    let mut used = Vec::new();
    cfg.capture_hotkey = sanitize_hotkey_option(cfg.capture_hotkey, &used);
    if let Some(b) = cfg.capture_hotkey {
        used.push(b);
    }
    cfg.overlay_hotkey = sanitize_hotkey_option(cfg.overlay_hotkey, &used);
    if let Some(b) = cfg.overlay_hotkey {
        used.push(b);
    }
    cfg.click_through_hotkey = sanitize_hotkey_option(cfg.click_through_hotkey, &used);
    cfg
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_file_path();
    save_config_to_path(cfg, &path)
}

pub fn save_config_to_path(cfg: &AppConfig, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    {
        let mut f = File::create(&tmp)
            .with_context(|| format!("Failed to create temp settings at {}", tmp.display()))?;
        f.write_all(json.as_bytes())
            .with_context(|| format!("Failed to write temp settings to {}", tmp.display()))?;
        f.flush()
            .with_context(|| format!("Failed to flush temp settings to {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("Failed to sync temp settings to {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to finalize settings at {}", path.display()))?;
    Ok(())
}

/// Validates and normalizes a model download URL.
/// Converts Hugging Face `/blob/` links automatically into `/resolve/` direct download links.
pub fn normalize_hf_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Model download link cannot be empty");
    }
    if !trimmed.starts_with("https://") {
        bail!("Model download link must start with https://");
    }

    let mut normalized = trimmed.to_string();
    if let Some(after_scheme) = normalized.strip_prefix("https://") {
        let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
        if !host.contains('@')
            && (host.eq_ignore_ascii_case("huggingface.co")
                || host.to_ascii_lowercase().ends_with(".huggingface.co"))
        {
            if let Some(idx) = normalized.find("/blob/") {
                normalized.replace_range(idx..idx + "/blob/".len(), "/resolve/");
            }
        }
    }

    Ok(normalized)
}

pub fn normalize_asr_url(raw: &str) -> Result<String> {
    let mut url = normalize_hf_url(raw)?;
    while url.ends_with('/') {
        url.pop();
    }
    Ok(url)
}

fn is_valid_hf_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && !seg
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | ':' | '\0'))
}

/// Extracts `(owner, repo)` from a Hugging Face URL such as `https://huggingface.co/owner/repo`
/// or `https://huggingface.co/owner/repo/tree/main`.
pub fn parse_hf_repo_id(url: &str) -> Option<(String, String)> {
    let without_frag = url.trim().split('#').next()?;
    let without_query = without_frag.split('?').next()?;
    let trimmed = without_query.trim_end_matches('/');
    let after_scheme = trimmed.strip_prefix("https://")?;
    let mut parts = after_scheme.split('/');
    let host = parts.next()?.trim();
    if host.contains('@')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
        || !(host.eq_ignore_ascii_case("huggingface.co")
            || host.to_ascii_lowercase().ends_with(".huggingface.co"))
    {
        return None;
    }
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if !is_valid_hf_segment(owner) || !is_valid_hf_segment(repo) {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

pub fn find_asr_preset_by_url(url: &str) -> Option<&'static AsrPreset> {
    let normalized = normalize_asr_url(url).ok()?;
    let target_repo = parse_hf_repo_id(&normalized);
    for preset in ASR_PRESETS {
        if preset.url.eq_ignore_ascii_case(&normalized) {
            return Some(preset);
        }
        if let (Some((o1, r1)), Some((o2, r2))) = (&target_repo, parse_hf_repo_id(preset.url)) {
            if o1.eq_ignore_ascii_case(&o2) && r1.eq_ignore_ascii_case(&r2) {
                return Some(preset);
            }
        }
    }
    None
}

fn sanitize_slug(raw: &str) -> String {
    let out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "custom-asr".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn asr_slug_from_url(url: &str) -> String {
    if let Some(preset) = find_asr_preset_by_url(url) {
        return preset.slug.to_string();
    }
    if let Some((owner, repo)) = parse_hf_repo_id(url) {
        return sanitize_slug(&format!("{owner}-{repo}"));
    }
    let without_query = url.split('?').next().unwrap_or(url);
    let stem = without_query
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("custom-asr");
    let stripped = stem
        .strip_suffix(".tar.bz2")
        .or_else(|| stem.strip_suffix(".onnx"))
        .unwrap_or(stem);
    sanitize_slug(stripped)
}

pub fn asr_model_dir(models_dir: &Path, url: &str) -> PathBuf {
    models_dir.join("asr").join(asr_slug_from_url(url))
}

pub fn display_asr_name(url: &str) -> String {
    if let Some(preset) = find_asr_preset_by_url(url) {
        return format!("{} ({})", preset.slug, preset.files.join(", "));
    }
    if let Some((owner, repo)) = parse_hf_repo_id(url) {
        return format!("{owner}/{repo}");
    }
    asr_slug_from_url(url)
}

pub fn silero_vad_path(models_dir: &Path) -> PathBuf {
    models_dir.join("vad").join("silero_vad.onnx")
}

fn raw_gguf_filename(normalized_url: &str) -> String {
    let without_fragment = normalized_url.split('#').next().unwrap_or(normalized_url);
    let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
    let segment = without_query
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("model.gguf");

    let sanitized: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "model.gguf".to_string()
    } else if sanitized.to_ascii_lowercase().ends_with(".gguf") {
        sanitized
    } else {
        format!("{sanitized}.gguf")
    }
}

pub fn display_llm_name(normalized_url: &str) -> String {
    raw_gguf_filename(normalized_url)
}

pub fn llm_model_path(models_dir: &Path, normalized_url: &str) -> PathBuf {
    models_dir.join("llm").join(raw_gguf_filename(normalized_url))
}

/// Checks `<models_dir>/llm/<file>` first, then `<models_dir>/<file>` for drop-in GGUFs.
pub fn resolve_existing_llm_path(models_dir: &Path, normalized_url: &str) -> Option<PathBuf> {
    let fname = raw_gguf_filename(normalized_url);
    let primary = models_dir.join("llm").join(&fname);
    if primary.is_file() {
        return Some(primary);
    }
    let flat = models_dir.join(&fname);
    if flat.is_file() {
        return Some(flat);
    }
    None
}

pub fn parse_hex_color(hex: &str) -> Option<[u8; 3]> {
    let s = hex.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

pub fn format_hex_color(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hf_blob_links_and_rejects_invalid() {
        let blob = "  https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/blob/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf?download=true  ";
        let norm = normalize_hf_url(blob).unwrap();
        assert_eq!(
            norm,
            "https://huggingface.co/HauhauCS/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive/resolve/main/Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf?download=true"
        );
        assert!(normalize_hf_url("").is_err());
        assert!(normalize_hf_url("ftp://example.com/a.gguf").is_err());
        assert!(normalize_hf_url("http://huggingface.co/a/b/resolve/main/m.gguf").is_err());
    }

    #[test]
    fn parse_hf_repo_id_strips_query_and_fragment_and_rejects_traversal() {
        assert_eq!(
            parse_hf_repo_id("https://huggingface.co/owner/repo?tab=files"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_hf_repo_id("https://huggingface.co/owner/repo#readme"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_hf_repo_id("https://huggingface.co/owner/repo/tree/main?view=tree#files"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(parse_hf_repo_id("https://huggingface.co/../repo"), None);
        assert_eq!(parse_hf_repo_id("https://huggingface.co/owner/.."), None);
        assert_eq!(parse_hf_repo_id("http://huggingface.co/owner/repo"), None);
        assert_eq!(parse_hf_repo_id("https://evil.com@huggingface.co/owner/repo"), None);
    }

    #[test]
    fn recognizes_asr_presets_with_trailing_slash_or_tree_main() {
        let p = find_asr_preset_by_url(
            "https://huggingface.co/oboroge0/Haas-ja-v1.0-onnx-quantized/tree/main/",
        )
        .expect("should match Haas preset");
        assert_eq!(p.slug, "haas-ja-v1.0-onnx-quantized");
        assert_eq!(p.arch, AsrArch::NemoTransducer { feature_dim: 128 });

        let sv = find_asr_preset_by_url(
            "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
        )
        .expect("should match SenseVoice preset");
        assert_eq!(sv.arch, AsrArch::SenseVoice);
    }

    #[test]
    fn builds_default_and_custom_model_paths() {
        let cfg = AppConfig::default();
        assert_eq!(effective_model_dir(&cfg), default_model_dir());
        assert!(default_model_dir().ends_with("models"));

        let custom_dir = Path::new("C:\\Models");
        assert_eq!(
            llm_model_path(
                custom_dir,
                "https://huggingface.co/a/b/resolve/main/qwen.gguf"
            ),
            PathBuf::from("C:\\Models\\llm\\qwen.gguf")
        );
        assert_eq!(ASR_PRESETS[0].url, DEFAULT_ASR_URL);
        assert_eq!(
            asr_model_dir(custom_dir, DEFAULT_ASR_URL),
            PathBuf::from("C:\\Models\\asr\\haas-ja-v1.0-onnx-quantized")
        );
        assert_eq!(
            silero_vad_path(custom_dir),
            PathBuf::from("C:\\Models\\vad\\silero_vad.onnx")
        );
    }

    #[test]
    fn settings_roundtrip_preserves_multi_monitor_and_negative_coords() {
        let mut cfg = AppConfig::default();
        cfg.main_window = Some(WindowState {
            x: -1440,
            y: -120,
            width: 560.0,
            height: 780.0,
            maximized: false,
        });
        cfg.overlay_window = Some(WindowState {
            x: -800,
            y: 900,
            width: 820.0,
            height: 150.0,
            maximized: false,
        });
        cfg.audio_vad.source_lang = "ko".to_string();
        cfg.audio_vad.silero_threshold_pct = 62.5;
        cfg.overlay.bg_opacity_pct = 45.0;

        let encoded = serde_json::to_string_pretty(&cfg).unwrap();
        let decoded: AppConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.main_window, cfg.main_window);
        assert_eq!(decoded.overlay_window, cfg.overlay_window);
        assert_eq!(decoded.audio_vad.source_lang, "ko");
        assert_eq!(decoded.audio_vad.silero_threshold_pct, 62.5);
        assert_eq!(decoded.overlay.bg_opacity_pct, 45.0);
    }

    #[test]
    fn hex_color_roundtrips() {
        assert_eq!(parse_hex_color("#34A853"), Some([0x34, 0xA8, 0x53]));
        assert_eq!(parse_hex_color("1e1e1e"), Some([0x1E, 0x1E, 0x1E]));
        assert_eq!(parse_hex_color("invalid"), None);
        assert_eq!(parse_hex_color("あい"), None);
        assert_eq!(parse_hex_color("#日本"), None);
        assert_eq!(parse_hex_color("+1+2+3"), None);
        assert_eq!(format_hex_color([0x34, 0xA8, 0x53]), "#34A853");
    }

    #[test]
    fn default_hotkeys_are_distinct_function_keys() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.capture_hotkey.unwrap().display_label(), "F8");
        assert_eq!(cfg.overlay_hotkey.unwrap().display_label(), "F9");
        assert_eq!(cfg.click_through_hotkey.unwrap().display_label(), "F10");
    }

    #[test]
    fn default_audio_vad_tuned_for_vtuber_gaming_streams() {
        let vad = AudioVadConfig::default();
        assert_eq!(vad.source_lang, "ja");
        assert_eq!(vad.target_lang, "en");
        assert_eq!(vad.context_pairs, 5);
        assert_eq!(vad.vad_mode, VadMode::Silero);
        assert_eq!(vad.silero_threshold_pct, 20.0);
        assert_eq!(vad.energy_threshold_pct, 1.5);
        assert_eq!(vad.min_speech_sec, 0.25);
        assert_eq!(vad.max_speech_sec, 8.0);
        assert_eq!(vad.silence_mode, SilenceMode::Adaptive);
        assert_eq!(vad.silence_dur_sec, 0.85);
        assert!(vad.incremental_asr);
        assert_eq!(vad.interim_interval_sec, 0.65);
    }

    #[test]
    fn recognizes_zh_ko_en_asr_presets() {
        let zh_p = find_asr_preset_by_url(
            "https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09",
        )
        .expect("Paraformer ZH preset");
        assert_eq!(zh_p.arch, AsrArch::Paraformer);

        let zh_fr = find_asr_preset_by_url(
            "https://huggingface.co/csukuangfj2/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25",
        )
        .expect("FireRedASR2 CTC preset");
        assert_eq!(zh_fr.arch, AsrArch::FireRedAsrCtc);

        let ko = find_asr_preset_by_url(
            "https://huggingface.co/k2-fsa/sherpa-onnx-zipformer-korean-2024-06-24",
        )
        .expect("Zipformer Korean preset");
        assert_eq!(ko.arch, AsrArch::ZipformerTransducer { feature_dim: 80 });

        let en_v2 = find_asr_preset_by_url(
            "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        )
        .expect("Parakeet TDT v2 EN preset");
        assert_eq!(en_v2.arch, AsrArch::NemoTransducer { feature_dim: 128 });

        let en_v3 = find_asr_preset_by_url(
            "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        )
        .expect("Parakeet TDT v3 preset");
        assert_eq!(en_v3.arch, AsrArch::NemoTransducer { feature_dim: 128 });
    }

    #[test]
    fn default_overlay_scroll_crawl_and_legacy_json_upgrade() {
        let ov = OverlayConfig::default();
        assert_eq!(ov.live_style, LiveSubtitleStyle::PrefixLockedFill);
        assert_eq!(ov.max_lines, 3);
        assert!(ov.smooth_scroll);
        assert_eq!(ov.min_display_sec, 0.75);
        assert_eq!(ov.lines.len(), 2);
        assert!(!ov.lines[0].visible);
        assert!(ov.lines[1].visible);

        let legacy_json = r#"{
            "overlay": {
                "live_style": "PrefixLockedFill",
                "show_previous_line": true,
                "min_display_sec": 1.2
            }
        }"#;
        let parsed: AppConfig = serde_json::from_str(legacy_json).expect("legacy JSON upgrades cleanly");
        assert_eq!(parsed.overlay.max_lines, 3);
        assert!(parsed.overlay.smooth_scroll);
        assert_eq!(parsed.overlay.min_display_sec, 1.2);
    }

    #[test]
    fn audio_vad_profiles_update_thresholds_while_preserving_identity() {
        let mut cfg = AudioVadConfig::default();
        cfg.loopback_device_id = Some("dev-out".to_string());
        cfg.mic_device_id = Some("dev-in".to_string());
        cfg.source_lang = "ko".to_string();
        cfg.target_lang = "vi".to_string();
        cfg.compute_backend = ComputeBackend::CpuOnly;

        cfg.apply_general_video_profile();
        assert_eq!(cfg.vad_mode, VadMode::Silero);
        assert_eq!(cfg.silero_threshold_pct, 42.0);
        assert_eq!(cfg.energy_threshold_pct, 1.5);
        assert_eq!(cfg.min_speech_sec, 0.25);
        assert_eq!(cfg.max_speech_sec, 8.0);
        assert_eq!(cfg.silence_dur_sec, 0.85);
        assert_eq!(cfg.context_pairs, 6);
        assert_eq!(cfg.loopback_device_id.as_deref(), Some("dev-out"));
        assert_eq!(cfg.source_lang, "ko");
        assert_eq!(cfg.target_lang, "vi");
        assert_eq!(cfg.compute_backend, ComputeBackend::CpuOnly);

        cfg.apply_gaming_stream_profile();
        assert_eq!(cfg.vad_mode, VadMode::Silero);
        assert_eq!(cfg.silero_threshold_pct, 62.0);
        assert_eq!(cfg.energy_threshold_pct, 2.5);
        assert_eq!(cfg.min_speech_sec, 0.30);
        assert_eq!(cfg.max_speech_sec, 6.5);
        assert_eq!(cfg.silence_dur_sec, 0.70);
        assert_eq!(cfg.context_pairs, 4);
        assert_eq!(cfg.mic_device_id.as_deref(), Some("dev-in"));
    }

    #[test]
    fn llm_presets_are_valid_unique_https_ggufs() {
        use std::collections::HashSet;

        assert_eq!(LLM_PRESETS.len(), 6);
        assert_eq!(LLM_PRESETS[0].url, DEFAULT_LLM_URL);

        let mut urls = HashSet::new();
        let mut names = HashSet::new();
        for preset in LLM_PRESETS {
            let norm = normalize_hf_url(preset.url).expect("preset URL must normalize");
            assert!(norm.starts_with("https://huggingface.co/"));
            let fname = display_llm_name(&norm);
            assert!(
                fname.to_ascii_lowercase().ends_with(".gguf"),
                "preset file must end with .gguf: {fname}"
            );
            assert!(urls.insert(norm), "duplicate preset URL: {}", preset.url);
            assert!(
                names.insert(fname.to_ascii_lowercase()),
                "duplicate preset filename: {fname}"
            );
        }
    }
}
