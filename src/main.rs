#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod asr;
mod audio;
mod config;
mod downloader;
mod hotkey;
mod icon;
mod icon_render;
mod llm;
mod monitor;
mod overlay;
mod taskbar;
mod telemetry;

use anyhow::{bail, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

struct InstanceMutex(HANDLE);

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn acquire_single_instance() -> Option<InstanceMutex> {
    unsafe {
        let handle =
            CreateMutexW(None, false, w!("Local\\TranslateLive_SingleInstance_Mutex")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            if let Ok(hwnd) = FindWindowW(None, w!("Translate Live")) {
                if !hwnd.0.is_null() {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            return None;
        }
        Some(InstanceMutex(handle))
    }
}

fn parse_self_test_model_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--model-dir" {
            if let Some(dir) = args.next() {
                let trimmed = dir.trim();
                if !trimmed.is_empty() {
                    return PathBuf::from(trimmed);
                }
            }
        }
    }
    config::effective_model_dir(&config::load_config())
}

fn run_self_test(smoke_only: bool) -> Result<()> {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }

    println!("=== Translate Live Self-Test ===");

    // Verify bundled icon assets
    let vp_icon = icon::app_viewport_icon(false);
    println!("Icon viewport : {}x{} ({} bytes)", vp_icon.width, vp_icon.height, vp_icon.rgba.len());
    let (hw, hh, _) = icon::header_icon_rgba(true);
    println!("Icon header   : {}x{}", hw, hh);

    // Verify multi-monitor enumeration & off-screen safeguard
    let monitors = monitor::enumerate_monitors();
    println!("Active monitors: {}", monitors.len());
    let ov_fallback = monitor::sanitize_window_rect(
        Some(config::WindowState {
            x: -99999,
            y: -99999,
            width: 820.0,
            height: 150.0,
            maximized: false,
        }),
        820,
        150,
        260,
        72,
        &monitors,
        monitor::FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
    );
    println!(
        "Overlay safeguard recovery: ({}, {}) {}x{}",
        ov_fallback.x, ov_fallback.y, ov_fallback.width, ov_fallback.height
    );

    // Verify WASAPI device enumeration
    let devs = audio::enumerate_audio_devices()?;
    println!(
        "WASAPI devices: {} outputs, {} inputs",
        devs.outputs.len(),
        devs.inputs.len()
    );

    // Verify telemetry
    let mut tel = telemetry::TelemetrySampler::new();
    let snap = tel.poll();
    println!(
        "Telemetry: GPU={}, RAM={:.1} MB, CPU={:.1}%",
        snap.gpu_name, snap.ram_mb, snap.cpu_pct
    );

    // Verify default model dir does NOT exist unless already created by user
    let def_dir = config::default_model_dir();
    println!(
        "Default models dir (not auto-created): {} (exists={})",
        def_dir.display(),
        def_dir.exists()
    );

    if smoke_only {
        println!("Smoke self-test PASSED.");
        return Ok(());
    }

    let model_dir = parse_self_test_model_dir();
    let asr_url = config::DEFAULT_ASR_URL;
    let llm_url = config::normalize_hf_url(config::DEFAULT_LLM_URL)?;

    println!("Model dir : {}", model_dir.display());
    if !matches!(
        downloader::inspect_asr_cache(&model_dir, asr_url),
        downloader::CacheStatus::Ready(_)
    ) {
        println!(
            "Downloading default ASR ({}) + Silero VAD...",
            config::display_asr_name(asr_url)
        );
        let cancel = Arc::new(AtomicBool::new(false));
        downloader::download_asr_model(asr_url, &model_dir, true, cancel, |_| {})?;
    }

    let llm_path = match config::resolve_existing_llm_path(&model_dir, &llm_url) {
        Some(p) => p,
        None => {
            println!(
                "Downloading default LLM ({})...",
                config::display_llm_name(&llm_url)
            );
            let cancel = Arc::new(AtomicBool::new(false));
            downloader::download_llm_model(&llm_url, &model_dir, cancel, |_| {})?
        }
    };

    let (asr_name, asr_ms) = asr::run_asr_benchmark(&model_dir, asr_url, "ja", 4)?;
    println!("ASR benchmark ({asr_name}): {asr_ms} ms");

    let mut llm_handle = llm::LlmHandle::spawn();
    let t0 = Instant::now();
    let info = llm_handle.load(llm_path, config::ComputeBackend::VulkanGpu)?;
    println!(
        "LLM loaded {} on {} in {:.2}s",
        info.model_name,
        info.backend_desc,
        t0.elapsed().as_secs_f64()
    );

    let res1 = llm_handle.translate(llm::TranslateRequest {
        utterance_id: 1,
        text: "やばい、このボスめっちゃ強いんだけど！".to_string(),
        source_lang: "ja".to_string(),
        target_lang: "en".to_string(),
        context_pairs: 4,
        forced_prefix: None,
        is_final: true,
    })?;
    println!(
        "TL1 ({} ms, {:.1} Tok/s): {}",
        res1.elapsed_ms, res1.tokens_per_sec, res1.translation
    );
    if res1.translation.trim().is_empty() {
        bail!("LLM returned empty translation");
    }

    let res2 = llm_handle.translate(llm::TranslateRequest {
        utterance_id: 2,
        text: "もう一回行こうぜ！".to_string(),
        source_lang: "ja".to_string(),
        target_lang: "en".to_string(),
        context_pairs: 4,
        forced_prefix: None,
        is_final: true,
    })?;
    println!(
        "TL2 ({} ms, {:.1} Tok/s): {}",
        res2.elapsed_ms, res2.tokens_per_sec, res2.translation
    );

    let res3 = llm_handle.translate(llm::TranslateRequest {
        utterance_id: 3,
        text: "待って待って待って待って！".to_string(),
        source_lang: "ja".to_string(),
        target_lang: "en".to_string(),
        context_pairs: 4,
        forced_prefix: None,
        is_final: true,
    })?;
    println!(
        "TL3 ({} ms, {:.1} Tok/s): {}",
        res3.elapsed_ms, res3.tokens_per_sec, res3.translation
    );
    if res3.translation.trim().is_empty() || res3.translation.chars().count() > 80 {
        bail!(
            "LLM repetition stress test failed (len={}): {}",
            res3.translation.chars().count(),
            res3.translation
        );
    }
    let words3: Vec<String> = res3
        .translation
        .split_whitespace()
        .map(llm::normalize_dedup_key)
        .filter(|w| !w.is_empty())
        .collect();
    if words3
        .windows(4)
        .any(|w| w.iter().all(|k| k == &w[0]))
    {
        bail!("LLM output still repeated a word 4+ times: {}", res3.translation);
    }

    let res4 = llm_handle.translate(llm::TranslateRequest {
        utterance_id: 4,
        text: "目が乾いてまばたきするの忘れてる！".to_string(),
        source_lang: "ja".to_string(),
        target_lang: "en".to_string(),
        context_pairs: 4,
        forced_prefix: Some("It's drying out".to_string()),
        is_final: true,
    })?;
    println!(
        "TL4 ({} ms, {:.1} Tok/s): {}",
        res4.elapsed_ms, res4.tokens_per_sec, res4.translation
    );
    if !res4.translation.starts_with("It's drying out")
        || res4.translation.matches("drying out").count() > 1
        || res4.translation.len() <= "It's drying out".len() + 4
    {
        bail!(
            "Forced-prefix continuation test failed: {}",
            res4.translation
        );
    }

    llm_handle.unload();
    llm_handle.shutdown();
    println!("Full self-test PASSED.");
    Ok(())
}

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--self-test") {
        let smoke = std::env::args().any(|a| a == "--smoke");
        if let Err(err) = run_self_test(smoke) {
            eprintln!("Self-test FAILED: {err:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let Some(_instance_lock) = acquire_single_instance() else {
        return Ok(());
    };

    let startup_cfg = config::load_config();
    let initial_size = startup_cfg
        .main_window
        .map(|w| [w.width.max(500.0), w.height.max(580.0)])
        .unwrap_or([580.0, 900.0]);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Translate Live")
            .with_inner_size(initial_size)
            .with_min_inner_size([500.0, 580.0])
            .with_icon(icon::app_viewport_icon(false)),
        ..Default::default()
    };

    eframe::run_native(
        "Translate Live",
        options,
        Box::new(|cc| Ok(Box::new(app::TranslateLiveApp::new(cc)))),
    )
}
