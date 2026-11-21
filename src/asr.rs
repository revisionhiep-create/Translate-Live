use anyhow::{anyhow, bail, Context, Result};
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig, VoiceActivityDetector,
};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::compute_rms;
use crate::config::{
    asr_model_dir, find_asr_preset_by_url, silero_vad_path, AsrArch, AudioVadConfig, SilenceMode,
    VadMode,
};

const SAMPLE_RATE: i32 = 16000;
const WINDOW_SAMPLES: usize = 512; // 32 ms at 16 kHz

#[derive(Debug, Clone)]
pub enum AsrEvent {
    Interim {
        utterance_id: u64,
        text: String,
        asr_ms: u128,
    },
    Final {
        utterance_id: u64,
        text: String,
        asr_ms: u128,
    },
    Error(String),
}

#[derive(Debug, Clone)]
pub enum DetectedAsrKind {
    NemoCtc {
        model: PathBuf,
        tokens: PathBuf,
    },
    NemoTransducer {
        encoder: PathBuf,
        decoder: PathBuf,
        joiner: PathBuf,
        tokens: PathBuf,
        feature_dim: i32,
    },
    ZipformerTransducer {
        encoder: PathBuf,
        decoder: PathBuf,
        joiner: PathBuf,
        tokens: PathBuf,
        feature_dim: i32,
    },
    SenseVoice {
        model: PathBuf,
        tokens: PathBuf,
    },
    Paraformer {
        model: PathBuf,
        tokens: PathBuf,
    },
    FireRedAsrCtc {
        model: PathBuf,
        tokens: PathBuf,
    },
    ZipformerCtc {
        model: PathBuf,
        tokens: PathBuf,
    },
}

fn find_file_matching(dir: &Path, mut pred: impl FnMut(&str) -> bool) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    let mut matches: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| pred(&n.to_ascii_lowercase()))
        })
        .collect();
    matches.sort();
    // Prefer int8 / quantized variants if multiple exist
    if let Some(q) = matches.iter().find(|p| {
        let l = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        l.contains(".int8.") || l.contains(".quantized.")
    }) {
        return Some(q.clone());
    }
    matches.into_iter().next()
}

pub fn detect_asr_spec(asr_dir: &Path, raw_url: &str) -> Result<DetectedAsrKind> {
    if !asr_dir.is_dir() {
        bail!("ASR model directory not found: {}", asr_dir.display());
    }

    let tokens = find_file_matching(asr_dir, |n| n == "tokens.txt")
        .or_else(|| find_file_matching(asr_dir, |n| n == "vocab.txt"))
        .ok_or_else(|| anyhow!("Missing tokens.txt in {}", asr_dir.display()))?;

    if let Some(preset) = find_asr_preset_by_url(raw_url) {
        return match preset.arch {
            AsrArch::NemoCtc => {
                let model = asr_dir.join("model.int8.onnx");
                let model = if model.is_file() {
                    model
                } else {
                    find_file_matching(asr_dir, |n| n.ends_with(".onnx"))
                        .ok_or_else(|| anyhow!("Missing .onnx model in {}", asr_dir.display()))?
                };
                Ok(DetectedAsrKind::NemoCtc { model, tokens })
            }
            AsrArch::NemoTransducer { feature_dim } => {
                let encoder = asr_dir.join("encoder.quantized.onnx");
                let encoder = if encoder.is_file() {
                    encoder
                } else {
                    find_file_matching(asr_dir, |n| n.starts_with("encoder") && n.ends_with(".onnx"))
                        .ok_or_else(|| anyhow!("Missing encoder*.onnx in {}", asr_dir.display()))?
                };
                let decoder = find_file_matching(asr_dir, |n| {
                    n.starts_with("decoder") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing decoder*.onnx in {}", asr_dir.display()))?;
                let joiner = find_file_matching(asr_dir, |n| {
                    n.starts_with("joiner") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing joiner*.onnx in {}", asr_dir.display()))?;
                Ok(DetectedAsrKind::NemoTransducer {
                    encoder,
                    decoder,
                    joiner,
                    tokens,
                    feature_dim,
                })
            }
            AsrArch::ZipformerTransducer { feature_dim } => {
                let encoder = find_file_matching(asr_dir, |n| {
                    n.starts_with("encoder") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing encoder*.onnx in {}", asr_dir.display()))?;
                let decoder = find_file_matching(asr_dir, |n| {
                    n.starts_with("decoder") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing decoder*.onnx in {}", asr_dir.display()))?;
                let joiner = find_file_matching(asr_dir, |n| {
                    n.starts_with("joiner") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing joiner*.onnx in {}", asr_dir.display()))?;
                Ok(DetectedAsrKind::ZipformerTransducer {
                    encoder,
                    decoder,
                    joiner,
                    tokens,
                    feature_dim,
                })
            }
            AsrArch::Paraformer => {
                let model = find_file_matching(asr_dir, |n| {
                    n.starts_with("model") && n.ends_with(".onnx")
                })
                .or_else(|| find_file_matching(asr_dir, |n| n.ends_with(".onnx")))
                .ok_or_else(|| anyhow!("Missing model*.onnx in {}", asr_dir.display()))?;
                Ok(DetectedAsrKind::Paraformer { model, tokens })
            }
            AsrArch::FireRedAsrCtc => {
                let model = find_file_matching(asr_dir, |n| {
                    n.starts_with("model") && n.ends_with(".onnx")
                })
                .or_else(|| find_file_matching(asr_dir, |n| n.ends_with(".onnx")))
                .ok_or_else(|| anyhow!("Missing model*.onnx in {}", asr_dir.display()))?;
                Ok(DetectedAsrKind::FireRedAsrCtc { model, tokens })
            }
            AsrArch::SenseVoice => {
                let model = find_file_matching(asr_dir, |n| {
                    n.starts_with("model") && n.ends_with(".onnx")
                })
                .ok_or_else(|| anyhow!("Missing model*.onnx in {}", asr_dir.display()))?;
                Ok(DetectedAsrKind::SenseVoice { model, tokens })
            }
        };
    }

    let hint = format!(
        "{} {}",
        raw_url.to_ascii_lowercase(),
        asr_dir.to_string_lossy().to_ascii_lowercase()
    );

    if let (Some(encoder), Some(decoder), Some(joiner)) = (
        find_file_matching(asr_dir, |n| n.starts_with("encoder") && n.ends_with(".onnx")),
        find_file_matching(asr_dir, |n| n.starts_with("decoder") && n.ends_with(".onnx")),
        find_file_matching(asr_dir, |n| n.starts_with("joiner") && n.ends_with(".onnx")),
    ) {
        if hint.contains("haas") || hint.contains("nemo") || hint.contains("parakeet") {
            let feature_dim = if hint.contains("haas") || hint.contains("tdt") {
                128
            } else {
                80
            };
            return Ok(DetectedAsrKind::NemoTransducer {
                encoder,
                decoder,
                joiner,
                tokens,
                feature_dim,
            });
        }
        return Ok(DetectedAsrKind::ZipformerTransducer {
            encoder,
            decoder,
            joiner,
            tokens,
            feature_dim: 80,
        });
    }

    let model = find_file_matching(asr_dir, |n| n.ends_with(".onnx"))
        .ok_or_else(|| anyhow!("No .onnx model file found in {}", asr_dir.display()))?;

    if hint.contains("fire-red") || hint.contains("firered") {
        Ok(DetectedAsrKind::FireRedAsrCtc { model, tokens })
    } else if hint.contains("sense") {
        Ok(DetectedAsrKind::SenseVoice { model, tokens })
    } else if hint.contains("paraformer") {
        Ok(DetectedAsrKind::Paraformer { model, tokens })
    } else if hint.contains("zipformer") {
        Ok(DetectedAsrKind::ZipformerCtc { model, tokens })
    } else {
        Ok(DetectedAsrKind::NemoCtc { model, tokens })
    }
}

pub fn build_offline_recognizer(
    asr_dir: &Path,
    raw_url: &str,
    source_lang: &str,
    num_threads: i32,
) -> Result<OfflineRecognizer> {
    let spec = detect_asr_spec(asr_dir, raw_url)?;
    let mut cfg = OfflineRecognizerConfig::default();
    cfg.model_config.num_threads = num_threads.clamp(1, 16);
    cfg.model_config.provider = Some("cpu".to_string());
    cfg.model_config.debug = false;

    match spec {
        DetectedAsrKind::NemoCtc { model, tokens } => {
            cfg.model_config.nemo_ctc.model = Some(model.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
        DetectedAsrKind::NemoTransducer {
            encoder,
            decoder,
            joiner,
            tokens,
            feature_dim,
        } => {
            cfg.feat_config.feature_dim = feature_dim;
            cfg.model_config.model_type = Some("nemo_transducer".to_string());
            cfg.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
            cfg.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
            cfg.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
        DetectedAsrKind::ZipformerTransducer {
            encoder,
            decoder,
            joiner,
            tokens,
            feature_dim,
        } => {
            cfg.feat_config.feature_dim = feature_dim;
            cfg.model_config.transducer.encoder = Some(encoder.to_string_lossy().into_owned());
            cfg.model_config.transducer.decoder = Some(decoder.to_string_lossy().into_owned());
            cfg.model_config.transducer.joiner = Some(joiner.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
        DetectedAsrKind::SenseVoice { model, tokens } => {
            let sv_lang = match source_lang {
                "ja" | "ko" | "zh" | "yue" | "en" | "auto" => source_lang,
                _ => "auto",
            };
            cfg.model_config.sense_voice.model = Some(model.to_string_lossy().into_owned());
            cfg.model_config.sense_voice.language = Some(sv_lang.to_string());
            cfg.model_config.sense_voice.use_itn = true;
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
        DetectedAsrKind::Paraformer { model, tokens } => {
            cfg.model_config.paraformer.model = Some(model.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
        DetectedAsrKind::FireRedAsrCtc { model, tokens } => {
            cfg.model_config.fire_red_asr_ctc.model = Some(model.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
            cfg.decoding_method = Some("greedy_search".to_string());
        }
        DetectedAsrKind::ZipformerCtc { model, tokens } => {
            cfg.model_config.zipformer_ctc.model = Some(model.to_string_lossy().into_owned());
            cfg.model_config.tokens = Some(tokens.to_string_lossy().into_owned());
        }
    }

    OfflineRecognizer::create(&cfg).ok_or_else(|| {
        anyhow!(
            "Failed to initialize sherpa-onnx recognizer from {}",
            asr_dir.display()
        )
    })
}

fn create_silero_vad(silero_path: &Path, cfg: &AudioVadConfig) -> Result<VoiceActivityDetector> {
    if !silero_path.is_file() {
        bail!(
            "Missing Silero VAD model at {}. Click Download Model first.",
            silero_path.display()
        );
    }
    let mut vad_cfg = VadModelConfig::default();
    vad_cfg.silero_vad.model = Some(silero_path.to_string_lossy().into_owned());
    vad_cfg.silero_vad.threshold = (cfg.silero_threshold_pct / 100.0).clamp(0.05, 0.95);
    vad_cfg.silero_vad.min_silence_duration = cfg.silence_dur_sec.clamp(0.10, 5.0);
    vad_cfg.silero_vad.min_speech_duration = cfg.min_speech_sec.clamp(0.10, 5.0);
    vad_cfg.silero_vad.max_speech_duration = cfg.max_speech_sec.clamp(1.0, 30.0);
    vad_cfg.silero_vad.window_size = WINDOW_SAMPLES as i32;
    vad_cfg.sample_rate = SAMPLE_RATE;
    vad_cfg.num_threads = 1;
    vad_cfg.provider = Some("cpu".to_string());
    vad_cfg.debug = false;

    VoiceActivityDetector::create(&vad_cfg, 30.0)
        .ok_or_else(|| anyhow!("Failed to initialize Silero VAD from {}", silero_path.display()))
}

pub fn normalize_transcript_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn cap_char_runs(text: &str, max_run: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut run = 0usize;
    for ch in text.chars() {
        if Some(ch) == prev {
            run += 1;
        } else {
            prev = Some(ch);
            run = 1;
        }
        if run <= max_run {
            out.push(ch);
        }
    }
    out
}

fn is_terminal_punct(c: char) -> bool {
    matches!(c, '!' | '?' | '.' | '…' | '！' | '？' | '。')
}

fn cap_spaced_word_runs(text: &str, max_run: usize) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.len() <= max_run {
        return text.to_string();
    }
    let mut kept: Vec<String> = Vec::with_capacity(tokens.len());
    let mut prev_key = String::new();
    let mut run = 0usize;
    for tok in tokens {
        let key = normalize_transcript_key(tok);
        if !key.is_empty() && key == prev_key {
            run += 1;
        } else {
            prev_key = key;
            run = 1;
        }
        if run <= max_run {
            kept.push(tok.to_string());
        } else if let Some(last) = kept.last_mut() {
            if let Some(end_p) = tok.chars().last().filter(|&c| is_terminal_punct(c)) {
                if !last.chars().last().is_some_and(is_terminal_punct) {
                    while last
                        .chars()
                        .last()
                        .is_some_and(|c| matches!(c, ',' | '、' | ';' | ':'))
                    {
                        last.pop();
                    }
                    last.push(end_p);
                }
            }
        }
    }
    kept.join(" ")
}

fn collapse_cjk_unit_repetitions(text: &str, max_keep: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 4 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        let mut matched_cursor: Option<usize> = None;
        let max_len = 12.min(chars.len() - i);
        for len in 1..=max_len {
            let unit = &chars[i..i + len];
            if !unit[0].is_alphanumeric() || !unit[len - 1].is_alphanumeric() {
                continue;
            }
            if len >= 2 && unit.iter().all(|&c| c == unit[0]) {
                continue;
            }

            let mut occs: Vec<(usize, Option<char>)> = vec![(i, None)];
            let mut cursor = i + len;
            loop {
                if let Some(&sep) = chars.get(cursor) {
                    if matches!(sep, ' ' | '、' | ',')
                        && chars.get(cursor + 1..cursor + 1 + len) == Some(unit)
                    {
                        occs.push((cursor + 1, Some(sep)));
                        cursor += 1 + len;
                        continue;
                    }
                }
                if chars.get(cursor..cursor + len) == Some(unit) {
                    if len == 1 {
                        // Single-char runs without separators are already handled by cap_char_runs(..., 4)
                        break;
                    }
                    occs.push((cursor, None));
                    cursor += len;
                    continue;
                }
                break;
            }

            if occs.len() >= 4 {
                for &(start, sep) in occs.iter().take(max_keep) {
                    if let Some(s) = sep {
                        out.push(s);
                    }
                    for &ch in &chars[start..start + len] {
                        out.push(ch);
                    }
                }
                matched_cursor = Some(cursor);
                break;
            }
        }

        if let Some(next_i) = matched_cursor {
            i = next_i;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub fn collapse_asr_repetitions(text: &str) -> String {
    let step1 = cap_char_runs(text.trim(), 4);
    let step2 = cap_spaced_word_runs(&step1, 3);
    collapse_cjk_unit_repetitions(&step2, 3)
}

pub fn clean_asr_transcript(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let has_content = trimmed.chars().any(|c| {
        !c.is_whitespace()
            && !matches!(
                c,
                '.' | ',' | '!' | '?' | '。' | '、' | '！' | '？' | '…' | '-' | '—' | '〜' | '~'
            )
    });
    if !has_content {
        return None;
    }
    let collapsed = collapse_asr_repetitions(trimmed);
    let cleaned = collapsed.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// 80th-percentile 32 ms frame RMS across a speech segment.
/// Used as a foreground loudness gate so brief pauses/padding don't penalize real speech
/// while quiet background game voices/BGM below the threshold are rejected.
pub fn foreground_rms(samples: &[f32]) -> f32 {
    let mut frame_rms: Vec<f32> = samples
        .chunks(WINDOW_SAMPLES)
        .filter(|c| c.len() >= WINDOW_SAMPLES / 4)
        .map(compute_rms)
        .collect();
    if frame_rms.is_empty() {
        return compute_rms(samples);
    }
    frame_rms.sort_by(f32::total_cmp);
    let idx = (((frame_rms.len() as f32) * 0.80).floor() as usize).min(frame_rms.len() - 1);
    frame_rms[idx]
}

fn decode_samples(recognizer: &OfflineRecognizer, samples: &[f32]) -> (Option<String>, u128) {
    if samples.len() < (SAMPLE_RATE as usize) / 10 {
        return (None, 0);
    }
    let started = Instant::now();
    let stream = recognizer.create_stream();
    stream.accept_waveform(SAMPLE_RATE, samples);
    recognizer.decode(&stream);
    let ms = started.elapsed().as_millis();
    let text = stream
        .get_result()
        .and_then(|r| clean_asr_transcript(&r.text));
    (text, ms)
}

/// Pure-Rust Energy-based VAD state machine with pre-speech lookback buffer.
pub struct EnergyVad {
    pre_roll: VecDeque<f32>,
    active: Vec<f32>,
    in_speech: bool,
    silence_samples: usize,
    threshold: f32,
    min_speech_samples: usize,
    max_speech_samples: usize,
    silence_limit_samples: usize,
    adaptive: bool,
}

impl EnergyVad {
    pub fn new(cfg: &AudioVadConfig) -> Self {
        let mut vad = Self {
            pre_roll: VecDeque::with_capacity(WINDOW_SAMPLES * 6),
            active: Vec::new(),
            in_speech: false,
            silence_samples: 0,
            threshold: 0.03,
            min_speech_samples: 6400,
            max_speech_samples: 128000,
            silence_limit_samples: 8000,
            adaptive: false,
        };
        vad.update_params(cfg);
        vad
    }

    pub fn update_params(&mut self, cfg: &AudioVadConfig) {
        self.threshold = (cfg.energy_threshold_pct / 100.0).clamp(0.002, 0.60);
        self.min_speech_samples =
            ((cfg.min_speech_sec * SAMPLE_RATE as f32).round() as usize).max(WINDOW_SAMPLES);
        self.max_speech_samples = ((cfg.max_speech_sec * SAMPLE_RATE as f32).round() as usize)
            .max(self.min_speech_samples + WINDOW_SAMPLES);
        self.silence_limit_samples =
            ((cfg.silence_dur_sec * SAMPLE_RATE as f32).round() as usize).max(WINDOW_SAMPLES);
        self.adaptive = cfg.silence_mode == SilenceMode::Adaptive;
    }

    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    pub fn active_samples(&self) -> &[f32] {
        &self.active
    }

    pub fn feed_chunk(&mut self, chunk: &[f32]) -> (f32, Option<Vec<f32>>) {
        let rms = compute_rms(chunk);
        let prob = (rms / (self.threshold * 2.0)).clamp(0.0, 1.0);
        let is_voice = rms >= self.threshold;

        if !self.in_speech {
            if is_voice {
                self.in_speech = true;
                self.silence_samples = 0;
                self.active.clear();
                self.active.extend(self.pre_roll.iter().copied());
                self.active.extend_from_slice(chunk);
                self.pre_roll.clear();
            } else {
                let max_pre = WINDOW_SAMPLES * 5;
                self.pre_roll.extend(chunk.iter().copied());
                while self.pre_roll.len() > max_pre {
                    self.pre_roll.pop_front();
                }
            }
            return (prob, None);
        }

        self.active.extend_from_slice(chunk);
        if is_voice {
            self.silence_samples = 0;
        } else {
            self.silence_samples += chunk.len();
        }

        let effective_silence_limit = if self.adaptive
            && self.active.len() > (self.max_speech_samples * 3) / 5
        {
            (self.silence_limit_samples / 2).max(WINDOW_SAMPLES * 2)
        } else {
            self.silence_limit_samples
        };

        let hit_max = self.active.len() >= self.max_speech_samples;
        let hit_silence = self.silence_samples >= effective_silence_limit;

        if hit_max || hit_silence {
            let mut seg = std::mem::take(&mut self.active);
            let trailing_silence = self.silence_samples;
            self.in_speech = false;
            self.silence_samples = 0;
            if seg.len() >= self.min_speech_samples {
                if !hit_max && seg.len() > self.silence_limit_samples / 2 {
                    let trim = (trailing_silence / 2).min(seg.len() - self.min_speech_samples);
                    seg.truncate(seg.len() - trim);
                }
                return (prob, Some(seg));
            }
        }

        (prob, None)
    }

    pub fn reset(&mut self) {
        self.in_speech = false;
        self.silence_samples = 0;
        self.active.clear();
        self.pre_roll.clear();
    }

    pub fn flush(&mut self) -> Option<Vec<f32>> {
        self.in_speech = false;
        self.silence_samples = 0;
        let seg = std::mem::take(&mut self.active);
        if seg.len() >= self.min_speech_samples {
            Some(seg)
        } else {
            None
        }
    }
}

pub struct AsrWorkerHandle {
    stop_flag: Arc<AtomicBool>,
    live_cfg: Arc<Mutex<AudioVadConfig>>,
    join: Option<JoinHandle<()>>,
}

impl AsrWorkerHandle {
    pub fn start(
        models_dir: PathBuf,
        asr_url: String,
        initial_cfg: AudioVadConfig,
        audio_rx: Receiver<Vec<f32>>,
        vad_prob_bits: Arc<AtomicU32>,
        on_event: Arc<dyn Fn(AsrEvent) + Send + Sync>,
    ) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let live_cfg = Arc::new(Mutex::new(initial_cfg.clone()));
        let asr_dir = asr_model_dir(&models_dir, &asr_url);
        let silero_path = silero_vad_path(&models_dir);

        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let stop_w = Arc::clone(&stop_flag);
        let cfg_w = Arc::clone(&live_cfg);

        let join = thread::Builder::new()
            .name("asr-vad-worker".to_string())
            .spawn(move || {
                asr_worker_loop(
                    asr_dir,
                    asr_url,
                    silero_path,
                    initial_cfg,
                    cfg_w,
                    stop_w,
                    audio_rx,
                    vad_prob_bits,
                    ready_tx,
                    on_event,
                );
            })
            .context("Failed to spawn ASR worker thread")?;

        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(Self {
                stop_flag,
                live_cfg,
                join: Some(join),
            }),
            Ok(Err(err)) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(err)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = join.join();
                Err(anyhow!("Timed out loading ASR/VAD models"))
            }
        }
    }

    pub fn update_config(&self, cfg: AudioVadConfig) {
        *self.live_cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AsrWorkerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn silero_params_changed(a: &AudioVadConfig, b: &AudioVadConfig) -> bool {
    (a.silero_threshold_pct - b.silero_threshold_pct).abs() > 0.1
        || (a.min_speech_sec - b.min_speech_sec).abs() > 0.01
        || (a.max_speech_sec - b.max_speech_sec).abs() > 0.01
        || (a.silence_dur_sec - b.silence_dur_sec).abs() > 0.01
}

#[allow(clippy::too_many_arguments)]
fn asr_worker_loop(
    asr_dir: PathBuf,
    asr_url: String,
    silero_path: PathBuf,
    mut active_cfg: AudioVadConfig,
    live_cfg: Arc<Mutex<AudioVadConfig>>,
    stop_flag: Arc<AtomicBool>,
    audio_rx: Receiver<Vec<f32>>,
    vad_prob_bits: Arc<AtomicU32>,
    ready_tx: Sender<Result<()>>,
    on_event: Arc<dyn Fn(AsrEvent) + Send + Sync>,
) {
    let recognizer = match build_offline_recognizer(
        &asr_dir,
        &asr_url,
        &active_cfg.source_lang,
        active_cfg.asr_threads,
    ) {
        Ok(r) => r,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let mut silero_vad = if active_cfg.vad_mode == VadMode::Silero {
        match create_silero_vad(&silero_path, &active_cfg) {
            Ok(v) => Some(v),
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                return;
            }
        }
    } else {
        None
    };

    let _ = ready_tx.send(Ok(()));

    let mut energy_vad = EnergyVad::new(&active_cfg);
    let mut applied_silero_cfg: Option<AudioVadConfig> = if silero_vad.is_some() {
        Some(active_cfg.clone())
    } else {
        None
    };
    let mut last_seen_vad_mode = active_cfg.vad_mode;
    let mut last_silero_attempt_cfg = active_cfg.clone();
    let mut chunk_buf: Vec<f32> = Vec::with_capacity(WINDOW_SAMPLES * 8);
    let mut silero_active_buf: Vec<f32> = Vec::with_capacity(SAMPLE_RATE as usize * 10);
    let mut speech_started = false;
    let mut speech_started_at = Instant::now();
    let mut last_interim_at = Instant::now();
    let mut last_interim_text = String::new();
    let mut smoothed_prob = 0.0f32;
    let mut quiet_start: Option<Instant> = None;
    let mut utterance_id: u64 = 1;
    let mut utterance_had_interim = false;
    let mut has_new_speech_since_interim = false;
    let mut pause_started_at: Option<Instant> = None;

    while !stop_flag.load(Ordering::Relaxed) {
        let samples = match audio_rx.recv_timeout(Duration::from_millis(40)) {
            Ok(s) => s,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Check for live VAD/timing updates from UI
        let latest = live_cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let ui_mode_changed = latest.vad_mode != last_seen_vad_mode;
        let silero_param_dirty = latest.vad_mode == VadMode::Silero
            && (applied_silero_cfg
                .as_ref()
                .is_some_and(|c| silero_params_changed(&latest, c))
                || (silero_vad.is_none() && silero_params_changed(&latest, &last_silero_attempt_cfg)));
        last_seen_vad_mode = latest.vad_mode;

        let mut next_cfg = latest;
        if ui_mode_changed || silero_param_dirty {
            if next_cfg.vad_mode == VadMode::Silero {
                last_silero_attempt_cfg = next_cfg.clone();
                match create_silero_vad(&silero_path, &next_cfg) {
                    Ok(v) => {
                        silero_vad = Some(v);
                        applied_silero_cfg = Some(next_cfg.clone());
                        if utterance_had_interim {
                            utterance_id += 1;
                            utterance_had_interim = false;
                        }
                        silero_active_buf.clear();
                        speech_started = false;
                        last_interim_text.clear();
                        has_new_speech_since_interim = false;
                        pause_started_at = None;
                        quiet_start = None;
                        energy_vad.reset();
                    }
                    Err(err) => {
                        on_event(AsrEvent::Error(err.to_string()));
                        if silero_vad.is_none() {
                            next_cfg.vad_mode = VadMode::Energy;
                        }
                    }
                }
            } else {
                silero_vad = None;
                applied_silero_cfg = None;
                if utterance_had_interim {
                    utterance_id += 1;
                    utterance_had_interim = false;
                }
                silero_active_buf.clear();
                speech_started = false;
                last_interim_text.clear();
                has_new_speech_since_interim = false;
                pause_started_at = None;
                quiet_start = None;
                energy_vad.reset();
            }
        } else if silero_vad.is_none() && next_cfg.vad_mode == VadMode::Silero {
            // Silero failed to load earlier and user has not changed mode/params; stay on Energy fallback.
            next_cfg.vad_mode = VadMode::Energy;
        }
        energy_vad.update_params(&next_cfg);
        active_cfg = next_cfg;

        chunk_buf.extend_from_slice(&samples);

        while chunk_buf.len() >= WINDOW_SAMPLES {
            let chunk: Vec<f32> = chunk_buf.drain(..WINDOW_SAMPLES).collect();
            let chunk_rms = compute_rms(&chunk);

            match active_cfg.vad_mode {
                VadMode::Silero => {
                    let Some(ref vad) = silero_vad else {
                        break;
                    };
                    vad.accept_waveform(&chunk);
                    let detected = vad.detected();

                    let inst_prob = if detected {
                        (0.75 + (chunk_rms * 5.0).min(0.25)).clamp(0.0, 1.0)
                    } else {
                        (chunk_rms / 0.04).clamp(0.0, 0.35)
                    };
                    smoothed_prob = smoothed_prob * 0.65 + inst_prob * 0.35;
                    vad_prob_bits.store(smoothed_prob.to_bits(), Ordering::Relaxed);

                    let energy_gate = (active_cfg.energy_threshold_pct / 100.0).clamp(0.0, 0.60);

                    if detected {
                        quiet_start = None;
                        has_new_speech_since_interim = true;
                        pause_started_at = None;
                        if !speech_started {
                            speech_started = true;
                            speech_started_at = Instant::now();
                            last_interim_at = Instant::now();
                            last_interim_text.clear();
                        }
                        silero_active_buf.extend_from_slice(&chunk);
                        let max_samples = (active_cfg.max_speech_sec * SAMPLE_RATE as f32) as usize;
                        if silero_active_buf.len() > max_samples {
                            let excess = silero_active_buf.len() - max_samples;
                            silero_active_buf.drain(..excess);
                        }
                    } else if speech_started {
                        silero_active_buf.extend_from_slice(&chunk);
                        pause_started_at.get_or_insert_with(Instant::now);
                        // Adaptive silence helper when approaching max_speech:
                        // rely on Silero non-speech state rather than raw RMS so continuous game BGM won't block flushing
                        if active_cfg.silence_mode == SilenceMode::Adaptive
                            && speech_started_at.elapsed().as_secs_f32()
                                > active_cfg.max_speech_sec * 0.6
                        {
                            let qs = quiet_start.get_or_insert_with(Instant::now);
                            if qs.elapsed().as_secs_f32() >= active_cfg.silence_dur_sec * 0.50 {
                                vad.flush();
                                quiet_start = None;
                            }
                        }
                    } else {
                        // Keep small 160ms pre-roll for interim preview responsiveness
                        silero_active_buf.extend_from_slice(&chunk);
                        let max_idle = WINDOW_SAMPLES * 5;
                        if silero_active_buf.len() > max_idle {
                            let excess = silero_active_buf.len() - max_idle;
                            silero_active_buf.drain(..excess);
                        }
                    }

                    // Drain finalized VAD segments
                    let mut emitted_final = false;
                    while !vad.is_empty() {
                        if let Some(segment) = vad.front() {
                            vad.pop();
                            let mut emitted_this_seg = false;
                            let samples = segment.samples();
                            if energy_gate <= 0.0 || foreground_rms(samples) >= energy_gate {
                                let (maybe_text, asr_ms) = decode_samples(&recognizer, samples);
                                if let Some(text) = maybe_text {
                                    on_event(AsrEvent::Final {
                                        utterance_id,
                                        text,
                                        asr_ms,
                                    });
                                    emitted_this_seg = true;
                                    emitted_final = true;
                                }
                            }
                            if emitted_this_seg || utterance_had_interim {
                                utterance_id += 1;
                                utterance_had_interim = false;
                            }
                        }
                        silero_active_buf.clear();
                        speech_started = false;
                        last_interim_text.clear();
                        quiet_start = None;
                        has_new_speech_since_interim = false;
                        pause_started_at = None;
                    }

                    // Incremental ASR updates while speech is ongoing (interval OR short speaking pause catch-up)
                    let min_interim_samples =
                        ((active_cfg.min_speech_sec.min(0.25)) * SAMPLE_RATE as f32) as usize;
                    let interim_due = last_interim_at.elapsed().as_secs_f32()
                        >= active_cfg.interim_interval_sec
                        || pause_started_at.is_some_and(|p| p.elapsed().as_secs_f32() >= 0.28);

                    if !emitted_final
                        && speech_started
                        && active_cfg.incremental_asr
                        && has_new_speech_since_interim
                        && interim_due
                        && silero_active_buf.len() >= min_interim_samples
                        && (energy_gate <= 0.0
                            || foreground_rms(&silero_active_buf) >= energy_gate)
                    {
                        has_new_speech_since_interim = false;
                        let (maybe_text, asr_ms) = decode_samples(&recognizer, &silero_active_buf);
                        last_interim_at = Instant::now();
                        if let Some(text) = maybe_text {
                            if normalize_transcript_key(&text)
                                != normalize_transcript_key(&last_interim_text)
                            {
                                last_interim_text = text.clone();
                                utterance_had_interim = true;
                                on_event(AsrEvent::Interim {
                                    utterance_id,
                                    text,
                                    asr_ms,
                                });
                            }
                        }
                    }
                }
                VadMode::Energy => {
                    let (inst_prob, maybe_seg) = energy_vad.feed_chunk(&chunk);
                    smoothed_prob = smoothed_prob * 0.65 + inst_prob * 0.35;
                    vad_prob_bits.store(smoothed_prob.to_bits(), Ordering::Relaxed);

                    if let Some(seg) = maybe_seg {
                        let (maybe_text, asr_ms) = decode_samples(&recognizer, &seg);
                        let mut emitted_this_seg = false;
                        if let Some(text) = maybe_text {
                            on_event(AsrEvent::Final {
                                utterance_id,
                                text,
                                asr_ms,
                            });
                            emitted_this_seg = true;
                        }
                        if emitted_this_seg || utterance_had_interim {
                            utterance_id += 1;
                            utterance_had_interim = false;
                        }
                        last_interim_text.clear();
                    } else if energy_vad.in_speech()
                        && active_cfg.incremental_asr
                        && last_interim_at.elapsed().as_secs_f32()
                            >= active_cfg.interim_interval_sec
                        && energy_vad.active_samples().len()
                            >= ((active_cfg.min_speech_sec.min(0.25)) * SAMPLE_RATE as f32)
                                as usize
                    {
                        let (maybe_text, asr_ms) =
                            decode_samples(&recognizer, energy_vad.active_samples());
                        last_interim_at = Instant::now();
                        if let Some(text) = maybe_text {
                            if normalize_transcript_key(&text)
                                != normalize_transcript_key(&last_interim_text)
                            {
                                last_interim_text = text.clone();
                                utterance_had_interim = true;
                                on_event(AsrEvent::Interim {
                                    utterance_id,
                                    text,
                                    asr_ms,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Flush trailing audio only when input stream terminated unexpectedly (not on explicit stop).
    if !stop_flag.load(Ordering::Relaxed) {
        let energy_gate = (active_cfg.energy_threshold_pct / 100.0).clamp(0.0, 0.60);
        if let Some(ref vad) = silero_vad {
            vad.flush();
            while !vad.is_empty() {
                if let Some(segment) = vad.front() {
                    vad.pop();
                    let samples = segment.samples();
                    if energy_gate <= 0.0 || foreground_rms(samples) >= energy_gate {
                        let (maybe_text, asr_ms) = decode_samples(&recognizer, samples);
                        if let Some(text) = maybe_text {
                            on_event(AsrEvent::Final {
                                utterance_id,
                                text,
                                asr_ms,
                            });
                            utterance_id += 1;
                        }
                    }
                }
            }
        } else if let Some(seg) = energy_vad.flush() {
            if energy_gate <= 0.0 || foreground_rms(&seg) >= energy_gate {
                let (maybe_text, asr_ms) = decode_samples(&recognizer, &seg);
                if let Some(text) = maybe_text {
                    on_event(AsrEvent::Final {
                        utterance_id,
                        text,
                        asr_ms,
                    });
                }
            }
        }
    }
    vad_prob_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
}

pub fn run_asr_benchmark(
    models_dir: &Path,
    asr_url: &str,
    source_lang: &str,
    asr_threads: i32,
) -> Result<(String, u128)> {
    let asr_dir = asr_model_dir(models_dir, asr_url);
    let recognizer = build_offline_recognizer(&asr_dir, asr_url, source_lang, asr_threads)?;
    // 2.0s synthetic speech-band test signal (16 kHz)
    let mut wave = Vec::with_capacity(SAMPLE_RATE as usize * 2);
    for i in 0..(SAMPLE_RATE as usize * 2) {
        let t = (i as f32) / (SAMPLE_RATE as f32);
        let env = ((t * 3.14).sin().abs()).min(1.0);
        let sig = ((t * 220.0 * std::f32::consts::TAU).sin() * 0.12
            + (t * 440.0 * std::f32::consts::TAU).sin() * 0.08)
            * env;
        wave.push(sig);
    }
    let started = Instant::now();
    let stream = recognizer.create_stream();
    stream.accept_waveform(SAMPLE_RATE, &wave);
    recognizer.decode(&stream);
    let _ = stream.get_result();
    let ms = started.elapsed().as_millis();
    let model_label = asr_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("ASR")
        .to_string();
    Ok((model_label, ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_asr_transcript_filters_punctuation_only_noise() {
        assert_eq!(clean_asr_transcript(""), None);
        assert_eq!(clean_asr_transcript("   。 、 ！ "), None);
        assert_eq!(clean_asr_transcript("..."), None);
        assert_eq!(
            clean_asr_transcript("  こんにちは！  "),
            Some("こんにちは！".to_string())
        );
    }

    #[test]
    fn energy_vad_segments_speech_burst_followed_by_silence() {
        let mut cfg = AudioVadConfig::default();
        cfg.energy_threshold_pct = 5.0; // threshold = 0.05
        cfg.min_speech_sec = 0.10;
        cfg.silence_dur_sec = 0.20;
        cfg.max_speech_sec = 5.0;

        let mut vad = EnergyVad::new(&cfg);
        let loud = vec![0.2f32; WINDOW_SAMPLES];
        let silent = vec![0.0f32; WINDOW_SAMPLES];

        // 15 chunks (~0.48s) of voice
        for _ in 0..15 {
            let (_prob, seg) = vad.feed_chunk(&loud);
            assert!(seg.is_none());
        }
        assert!(vad.in_speech());

        // Silence chunks until end-of-speech triggers
        let mut finalized = None;
        for _ in 0..12 {
            let (_prob, seg) = vad.feed_chunk(&silent);
            if seg.is_some() {
                finalized = seg;
                break;
            }
        }
        let seg = finalized.expect("EnergyVad should emit speech segment after silence");
        assert!(seg.len() >= 1600);
        assert!(!vad.in_speech());
    }

    #[test]
    fn energy_vad_trims_trailing_silence() {
        let mut cfg = AudioVadConfig::default();
        cfg.energy_threshold_pct = 5.0;
        cfg.min_speech_sec = 0.20;
        cfg.silence_dur_sec = 0.50; // 8000 samples (~16 chunks)
        cfg.max_speech_sec = 5.0;
        cfg.silence_mode = SilenceMode::Fixed;

        let mut vad = EnergyVad::new(&cfg);
        let loud = vec![0.2f32; WINDOW_SAMPLES];
        let silent = vec![0.0f32; WINDOW_SAMPLES];

        // 10 voiced chunks = 5120 samples
        for _ in 0..10 {
            let (_, seg) = vad.feed_chunk(&loud);
            assert!(seg.is_none());
        }
        // Feed silence until emission (16 chunks = 8192 silence samples)
        let mut finalized = None;
        let mut silence_chunks = 0usize;
        for _ in 0..20 {
            silence_chunks += 1;
            let (_, seg) = vad.feed_chunk(&silent);
            if seg.is_some() {
                finalized = seg;
                break;
            }
        }
        let seg = finalized.expect("EnergyVad should emit speech");
        let untrimmed = (10 + silence_chunks) * WINDOW_SAMPLES;
        assert!(
            seg.len() < untrimmed,
            "expected trailing silence trimmed (got {}, untrimmed {untrimmed})",
            seg.len()
        );
    }

    #[test]
    fn foreground_rms_distinguishes_foreground_speech_from_quiet_bgm() {
        // 1.5s padded utterance: ~0.6s foreground speech (~0.12 RMS) + ~0.9s quiet padding (0.005 RMS)
        let mut speech = vec![0.005f32; SAMPLE_RATE as usize * 3 / 2];
        for s in &mut speech[3200..12800] {
            *s = 0.12;
        }
        assert!(foreground_rms(&speech) >= 0.08);

        // Background-only noise / distant game audio (~0.012 RMS)
        let bgm = vec![0.012f32; SAMPLE_RATE as usize];
        assert!(foreground_rms(&bgm) < 0.02);
    }

    #[test]
    fn detect_asr_spec_handles_zh_ko_en_presets() {
        let base = std::env::temp_dir().join(format!("tl_asr_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);

        // 1. Paraformer ZH
        let zh_p = base.join("paraformer");
        fs::create_dir_all(&zh_p).unwrap();
        fs::write(zh_p.join("model.int8.onnx"), b"onnx").unwrap();
        fs::write(zh_p.join("tokens.txt"), b"tok").unwrap();
        assert!(matches!(
            detect_asr_spec(
                &zh_p,
                "https://huggingface.co/csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09"
            )
            .unwrap(),
            DetectedAsrKind::Paraformer { .. }
        ));

        // 2. FireRedASR2 CTC ZH/EN
        let zh_fr = base.join("firered");
        fs::create_dir_all(&zh_fr).unwrap();
        fs::write(zh_fr.join("model.int8.onnx"), b"onnx").unwrap();
        fs::write(zh_fr.join("tokens.txt"), b"tok").unwrap();
        assert!(matches!(
            detect_asr_spec(
                &zh_fr,
                "https://huggingface.co/csukuangfj2/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25"
            )
            .unwrap(),
            DetectedAsrKind::FireRedAsrCtc { .. }
        ));

        // 3. Zipformer Korean
        let ko = base.join("zipformer_ko");
        fs::create_dir_all(&ko).unwrap();
        fs::write(ko.join("encoder-epoch-99-avg-1.int8.onnx"), b"onnx").unwrap();
        fs::write(ko.join("decoder-epoch-99-avg-1.onnx"), b"onnx").unwrap();
        fs::write(ko.join("joiner-epoch-99-avg-1.int8.onnx"), b"onnx").unwrap();
        fs::write(ko.join("tokens.txt"), b"tok").unwrap();
        assert!(matches!(
            detect_asr_spec(
                &ko,
                "https://huggingface.co/k2-fsa/sherpa-onnx-zipformer-korean-2024-06-24"
            )
            .unwrap(),
            DetectedAsrKind::ZipformerTransducer {
                feature_dim: 80,
                ..
            }
        ));

        // 4. Parakeet TDT 0.6B v2 EN
        let en = base.join("parakeet_v2");
        fs::create_dir_all(&en).unwrap();
        fs::write(en.join("encoder.int8.onnx"), b"onnx").unwrap();
        fs::write(en.join("decoder.int8.onnx"), b"onnx").unwrap();
        fs::write(en.join("joiner.int8.onnx"), b"onnx").unwrap();
        fs::write(en.join("tokens.txt"), b"tok").unwrap();
        assert!(matches!(
            detect_asr_spec(
                &en,
                "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"
            )
            .unwrap(),
            DetectedAsrKind::NemoTransducer {
                feature_dim: 128,
                ..
            }
        ));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn collapse_asr_repetitions_caps_excessive_loops() {
        assert_eq!(
            collapse_asr_repetitions("待って待って待って待って待って待って！"),
            "待って待って待って！"
        );
        assert_eq!(
            collapse_asr_repetitions("no no no no no no"),
            "no no no"
        );
        assert_eq!(
            collapse_asr_repetitions("ああああああああ"),
            "ああああ"
        );
        assert_eq!(
            collapse_asr_repetitions("えーと、ちょっと待ってね！"),
            "えーと、ちょっと待ってね！"
        );
    }

    #[test]
    fn normalize_transcript_key_ignores_punctuation_jitter() {
        assert_eq!(
            normalize_transcript_key("えーと"),
            normalize_transcript_key("えーと、")
        );
    }
}
