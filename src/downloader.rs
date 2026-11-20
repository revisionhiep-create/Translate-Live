use anyhow::{anyhow, bail, Context, Result};
use bzip2::read::BzDecoder;
use reqwest::blocking::Client;
use reqwest::header::{HeaderValue, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tar::Archive;

use crate::config::{
    asr_model_dir, find_asr_preset_by_url, llm_model_path, normalize_asr_url, normalize_hf_url,
    parse_hf_repo_id, resolve_existing_llm_path, silero_vad_path, SILERO_VAD_URL,
};

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    NotDownloaded,
    Partial(u64),
    Ready(u64),
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub name: String,
    pub category: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub is_partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileValidation {
    GgufMagic,
    RejectHtml,
}

#[derive(Debug, Deserialize)]
struct HfTreeEntry {
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(rename = "type", default)]
    entry_type: String,
}

fn dir_size_recursive(dir: &Path) -> u64 {
    let mut sum = 0u64;
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                sum = sum.saturating_add(dir_size_recursive(&p));
            } else {
                sum = sum.saturating_add(meta.len());
            }
        }
    }
    sum
}

fn part_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn is_part_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.ends_with(".part"))
}

fn verify_part_file_if_exists(part_path: &Path, validation: FileValidation) -> Result<u64> {
    if !part_path.is_file() {
        return Ok(0);
    }
    let len = fs::metadata(part_path)?.len();
    if len < 4 {
        let _ = fs::remove_file(part_path);
        return Ok(0);
    }
    if validation == FileValidation::GgufMagic {
        let mut f = File::open(part_path)?;
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic)?;
        if &magic != b"GGUF" {
            let _ = fs::remove_file(part_path);
            return Ok(0);
        }
    }
    Ok(len)
}

fn parse_total_from_content_range(header: Option<&HeaderValue>) -> Option<u64> {
    let text = header?.to_str().ok()?;
    let slash_pos = text.rfind('/')?;
    let total_str = text[slash_pos + 1..].trim();
    if total_str == "*" {
        return None;
    }
    total_str.parse::<u64>().ok()
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("translate-live/1.0")
        .connect_timeout(Duration::from_secs(30))
        .timeout(None)
        .build()
        .context("Failed to build HTTP client")
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype html") || trimmed.starts_with("<html")
}

pub fn download_file_resumable(
    client: &Client,
    url: &str,
    dest: &Path,
    validation: FileValidation,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(u64, Option<u64>, f64),
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create folder {}", parent.display()))?;
    }

    let part_path = part_path_for(dest);
    let existing_len = verify_part_file_if_exists(&part_path, validation)?;

    let mut req = client.get(url);
    if existing_len > 0 {
        req = req.header(RANGE, format!("bytes={existing_len}-"));
    }

    let mut resp = req
        .send()
        .with_context(|| format!("Failed to connect to {url}"))?;

    if existing_len > 0 && resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = fs::remove_file(&part_path);
        resp = client
            .get(url)
            .send()
            .with_context(|| format!("Failed to reconnect to {url}"))?;
    }

    if !resp.status().is_success() {
        bail!("Download failed (HTTP {}) for {url}", resp.status());
    }

    let (mut file, mut downloaded, total, mut checked_header) =
        if resp.status() == StatusCode::PARTIAL_CONTENT && existing_len > 0 {
            let total = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE))
                .or_else(|| resp.content_length().map(|rem| existing_len + rem));
            let mut f = OpenOptions::new()
                .write(true)
                .open(&part_path)
                .with_context(|| format!("Failed to open {}", part_path.display()))?;
            f.seek(SeekFrom::End(0))?;
            (f, existing_len, total, true)
        } else {
            let total = resp.content_length();
            let f = File::create(&part_path)
                .with_context(|| format!("Failed to create {}", part_path.display()))?;
            (f, 0u64, total, false)
        };

    let mut header_buf = [0u8; 64];
    let mut header_fill = 0usize;
    let mut buf = vec![0u8; 256 * 1024];

    let mut last_emit = Instant::now();
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let mut current_speed = 0.0f64;

    on_progress(downloaded, total, 0.0);

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            bail!("Download cancelled");
        }

        let n = resp
            .read(&mut buf)
            .context("Network error while downloading file")?;
        if n == 0 {
            break;
        }

        if !checked_header {
            let take = (header_buf.len() - header_fill).min(n);
            header_buf[header_fill..header_fill + take].copy_from_slice(&buf[..take]);
            header_fill += take;

            match validation {
                FileValidation::GgufMagic if header_fill >= 4 => {
                    if &header_buf[..4] != b"GGUF" {
                        drop(file);
                        let _ = fs::remove_file(&part_path);
                        bail!("URL did not return a valid GGUF model file (check the Hugging Face link)");
                    }
                    checked_header = true;
                }
                FileValidation::RejectHtml if header_fill >= 16 => {
                    if looks_like_html(&header_buf[..header_fill]) {
                        drop(file);
                        let _ = fs::remove_file(&part_path);
                        bail!("URL returned an HTML page instead of a model file ({url})");
                    }
                    checked_header = true;
                }
                _ => {}
            }
        }

        file.write_all(&buf[..n])
            .context("Failed writing bytes to disk")?;

        downloaded += n as u64;
        window_bytes += n as u64;

        let win_elapsed = window_start.elapsed().as_secs_f64();
        if win_elapsed >= 0.35 {
            current_speed = (window_bytes as f64) / win_elapsed;
            window_start = Instant::now();
            window_bytes = 0;
        }

        if last_emit.elapsed() >= Duration::from_millis(90) {
            on_progress(downloaded, total, current_speed);
            last_emit = Instant::now();
        }
    }

    if validation == FileValidation::GgufMagic && !checked_header {
        drop(file);
        let _ = fs::remove_file(&part_path);
        bail!("Downloaded file was too small to be a valid GGUF model");
    }
    if validation == FileValidation::RejectHtml && header_fill > 0 && looks_like_html(&header_buf[..header_fill]) {
        drop(file);
        let _ = fs::remove_file(&part_path);
        bail!("URL returned an HTML error page instead of model data");
    }

    if let Some(expected) = total {
        if downloaded < expected {
            bail!("Download ended early ({downloaded} of {expected} bytes received)");
        }
    }

    file.flush()?;
    file.sync_all()?;
    drop(file);

    on_progress(downloaded, total.or(Some(downloaded)), current_speed);

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&part_path, dest).with_context(|| {
        format!(
            "Failed to finalize downloaded file {} -> {}",
            part_path.display(),
            dest.display()
        )
    })?;

    Ok(downloaded)
}

pub fn download_llm_model(
    raw_url: &str,
    models_dir: &Path,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let norm = normalize_hf_url(raw_url)?;
    let dest = llm_model_path(models_dir, &norm);
    if dest.is_file() {
        return Ok(dest);
    }
    let client = build_http_client()?;
    let label = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("model.gguf")
        .to_string();
    download_file_resumable(
        &client,
        &norm,
        &dest,
        FileValidation::GgufMagic,
        &cancel,
        |downloaded, total, speed_bps| {
            on_progress(DownloadProgress {
                downloaded,
                total,
                speed_bps,
                label: label.clone(),
            });
        },
    )?;
    Ok(dest)
}

fn fetch_hf_repo_tree(client: &Client, owner: &str, repo: &str) -> Result<Vec<HfTreeEntry>> {
    let api_url = format!("https://huggingface.co/api/models/{owner}/{repo}/tree/main");
    let resp = client
        .get(&api_url)
        .timeout(Duration::from_secs(20))
        .send()
        .with_context(|| format!("Failed querying Hugging Face repo {owner}/{repo}"))?;
    if !resp.status().is_success() {
        bail!(
            "Hugging Face API returned {} for {owner}/{repo}",
            resp.status()
        );
    }
    let entries: Vec<HfTreeEntry> = resp
        .json()
        .with_context(|| format!("Failed parsing file list for {owner}/{repo}"))?;
    Ok(entries)
}

fn select_custom_asr_files(entries: &[HfTreeEntry]) -> Vec<(String, u64)> {
    let files: Vec<&HfTreeEntry> = entries
        .iter()
        .filter(|e| e.entry_type != "directory")
        .collect();

    let names: HashSet<&str> = files.iter().map(|e| e.path.as_str()).collect();
    let has_quant_model = names.iter().any(|n| {
        let l = n.to_ascii_lowercase();
        l == "model.int8.onnx" || l == "model.quantized.onnx"
    });
    let has_quant_encoder = names.iter().any(|n| {
        let l = n.to_ascii_lowercase();
        l.starts_with("encoder") && (l.contains(".int8.") || l.contains(".quantized.")) && l.ends_with(".onnx")
    });
    let has_quant_decoder = names.iter().any(|n| {
        let l = n.to_ascii_lowercase();
        l.starts_with("decoder") && l.contains(".int8.") && l.ends_with(".onnx")
    });
    let has_quant_joiner = names.iter().any(|n| {
        let l = n.to_ascii_lowercase();
        l.starts_with("joiner") && l.contains(".int8.") && l.ends_with(".onnx")
    });

    let mut chosen = Vec::new();
    for f in files {
        let lower = f.path.to_ascii_lowercase();
        let is_tokens = lower == "tokens.txt" || lower == "bpe.model" || lower == "vocab.txt";
        let is_onnx = lower.ends_with(".onnx");
        let is_data = lower.ends_with(".data");
        if !is_tokens && !is_onnx && !is_data {
            continue;
        }
        if is_onnx {
            if has_quant_model && lower == "model.onnx" {
                continue;
            }
            if has_quant_encoder
                && lower.starts_with("encoder")
                && !lower.contains(".int8.")
                && !lower.contains(".quantized.")
            {
                continue;
            }
            if has_quant_decoder && lower.starts_with("decoder") && !lower.contains(".int8.") {
                continue;
            }
            if has_quant_joiner && lower.starts_with("joiner") && !lower.contains(".int8.") {
                continue;
            }
        }
        chosen.push((f.path.clone(), f.size));
    }
    chosen
}

fn flatten_single_subdir(dest_dir: &Path) -> Result<()> {
    let entries: Vec<PathBuf> = fs::read_dir(dest_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    if entries.len() == 1 && entries[0].is_dir() {
        let inner = &entries[0];
        for child in fs::read_dir(inner)?.flatten() {
            let from = child.path();
            let to = dest_dir.join(child.file_name());
            if to.exists() {
                if to.is_dir() {
                    let _ = fs::remove_dir_all(&to);
                } else {
                    let _ = fs::remove_file(&to);
                }
            }
            fs::rename(&from, &to)?;
        }
        let _ = fs::remove_dir(inner);
    }
    Ok(())
}

pub fn download_asr_model(
    raw_url: &str,
    models_dir: &Path,
    ensure_silero_vad: bool,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let norm = normalize_asr_url(raw_url)?;
    let client = build_http_client()?;
    let dest_dir = asr_model_dir(models_dir, &norm);
    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create ASR dir {}", dest_dir.display()))?;

    // Check if it's a direct .tar.bz2 archive link
    let lower_no_query = norm
        .split('?')
        .next()
        .unwrap_or(&norm)
        .to_ascii_lowercase();
    if lower_no_query.ends_with(".tar.bz2") {
        let archive_path = dest_dir.join("model_archive.tar.bz2");
        download_file_resumable(
            &client,
            &norm,
            &archive_path,
            FileValidation::RejectHtml,
            &cancel,
            |downloaded, total, speed_bps| {
                on_progress(DownloadProgress {
                    downloaded,
                    total,
                    speed_bps,
                    label: "ASR archive (.tar.bz2)".to_string(),
                });
            },
        )?;
        on_progress(DownloadProgress {
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            label: "Extracting ASR archive...".to_string(),
        });
        let f = File::open(&archive_path)?;
        let dec = BzDecoder::new(f);
        let mut ar = Archive::new(dec);
        ar.unpack(&dest_dir)
            .context("Failed to extract .tar.bz2 ASR archive")?;
        let _ = fs::remove_file(&archive_path);
        flatten_single_subdir(&dest_dir)?;

        if ensure_silero_vad {
            ensure_silero_vad_downloaded(&client, models_dir, &cancel, &mut on_progress)?;
        }
        return Ok(dest_dir);
    }

    // Build file list: (download_url, local_path, known_size)
    let mut plan: Vec<(String, PathBuf, Option<u64>)> = Vec::new();
    if let Some(preset) = find_asr_preset_by_url(&norm) {
        let (owner, repo) = parse_hf_repo_id(preset.url)
            .ok_or_else(|| anyhow!("Invalid preset HF URL: {}", preset.url))?;
        let size_map = fetch_hf_repo_tree(&client, &owner, &repo)
            .ok()
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| (e.path, e.size))
                    .collect::<std::collections::HashMap<String, u64>>()
            })
            .unwrap_or_default();

        for &fname in preset.files {
            let f_dest = dest_dir.join(fname);
            let f_url = format!("https://huggingface.co/{owner}/{repo}/resolve/main/{fname}");
            let known_sz = size_map.get(fname).copied();
            plan.push((f_url, f_dest, known_sz));
        }
    } else if lower_no_query.ends_with(".onnx") {
        let fname = norm
            .split('?')
            .next()
            .unwrap_or(&norm)
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("model.onnx");
        plan.push((norm.clone(), dest_dir.join(fname), None));
    } else if let Some((owner, repo)) = parse_hf_repo_id(&norm) {
        let entries = fetch_hf_repo_tree(&client, &owner, &repo)?;
        let selected = select_custom_asr_files(&entries);
        if selected.is_empty() {
            bail!("No ONNX ASR model files found in {owner}/{repo}");
        }
        for (fname, sz) in selected {
            let f_dest = dest_dir.join(&fname);
            let f_url = format!("https://huggingface.co/{owner}/{repo}/resolve/main/{fname}");
            plan.push((f_url, f_dest, Some(sz)));
        }
    } else {
        bail!("Unsupported ASR URL format. Paste a Hugging Face model repo URL or .tar.bz2 link.");
    }

    let vad_dest = silero_vad_path(models_dir);
    let need_vad = ensure_silero_vad && !vad_dest.is_file();
    let vad_est = if need_vad { Some(1_807_522u64) } else { None };
    if need_vad {
        plan.push((SILERO_VAD_URL.to_string(), vad_dest, vad_est));
    }

    let known_total: Option<u64> = if plan.iter().all(|(_, _, s)| s.is_some()) {
        Some(plan.iter().filter_map(|(_, _, s)| *s).sum())
    } else {
        None
    };

    let mut completed_bytes = 0u64;
    for (f_url, f_dest, known_sz) in &plan {
        let fname = f_dest
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();

        if f_dest.is_file() {
            let existing = fs::metadata(f_dest).map(|m| m.len()).unwrap_or(0);
            completed_bytes = completed_bytes.saturating_add(known_sz.unwrap_or(existing));
            on_progress(DownloadProgress {
                downloaded: completed_bytes,
                total: known_total,
                speed_bps: 0.0,
                label: fname,
            });
            continue;
        }

        let base_done = completed_bytes;
        let received = download_file_resumable(
            &client,
            f_url,
            f_dest,
            FileValidation::RejectHtml,
            &cancel,
            |cur_dl, cur_tot, speed_bps| {
                let agg_total = known_total.or(cur_tot.map(|t| base_done + t));
                on_progress(DownloadProgress {
                    downloaded: base_done.saturating_add(cur_dl),
                    total: agg_total,
                    speed_bps,
                    label: fname.clone(),
                });
            },
        )?;
        completed_bytes = completed_bytes.saturating_add(known_sz.unwrap_or(received));
    }

    Ok(dest_dir)
}

pub fn ensure_silero_vad_only(
    models_dir: &Path,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let client = build_http_client()?;
    ensure_silero_vad_downloaded(&client, models_dir, &cancel, &mut on_progress)
}

fn ensure_silero_vad_downloaded(
    client: &Client,
    models_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let dest = silero_vad_path(models_dir);
    if dest.is_file() {
        return Ok(dest);
    }
    download_file_resumable(
        client,
        SILERO_VAD_URL,
        &dest,
        FileValidation::RejectHtml,
        cancel,
        |downloaded, total, speed_bps| {
            on_progress(DownloadProgress {
                downloaded,
                total,
                speed_bps,
                label: "silero_vad.onnx".to_string(),
            });
        },
    )?;
    Ok(dest)
}

pub fn inspect_asr_cache(models_dir: &Path, raw_url: &str) -> CacheStatus {
    let Ok(norm) = normalize_asr_url(raw_url) else {
        return CacheStatus::NotDownloaded;
    };
    let dir = asr_model_dir(models_dir, &norm);
    if !dir.is_dir() {
        return CacheStatus::NotDownloaded;
    }

    if let Some(preset) = find_asr_preset_by_url(&norm) {
        let mut ready_bytes = 0u64;
        let mut partial_bytes = 0u64;
        let mut all_present = true;
        for &f in preset.files {
            let p = dir.join(f);
            if p.is_file() {
                let sz = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if sz > 0 {
                    ready_bytes = ready_bytes.saturating_add(sz);
                } else {
                    all_present = false;
                }
            } else {
                all_present = false;
                let part = part_path_for(&p);
                if let Ok(m) = fs::metadata(&part) {
                    partial_bytes = partial_bytes.saturating_add(m.len());
                }
            }
        }
        if all_present && ready_bytes > 0 {
            return CacheStatus::Ready(ready_bytes);
        }
        let total_partial = ready_bytes.saturating_add(partial_bytes);
        return if total_partial > 0 {
            CacheStatus::Partial(total_partial)
        } else {
            CacheStatus::NotDownloaded
        };
    }

    let mut has_tokens = false;
    let mut has_onnx = false;
    let mut ready_bytes = 0u64;
    let mut partial_bytes = 0u64;

    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if is_part_file(&p) {
                    partial_bytes = partial_bytes.saturating_add(sz);
                } else {
                    ready_bytes = ready_bytes.saturating_add(sz);
                    if name == "tokens.txt" || name == "vocab.txt" || name == "bpe.model" {
                        has_tokens = true;
                    }
                    if name.ends_with(".onnx") {
                        has_onnx = true;
                    }
                }
            }
        }
    }

    if has_tokens && has_onnx && partial_bytes == 0 && ready_bytes > 0 {
        CacheStatus::Ready(ready_bytes)
    } else if ready_bytes + partial_bytes > 0 {
        CacheStatus::Partial(ready_bytes + partial_bytes)
    } else {
        CacheStatus::NotDownloaded
    }
}

pub fn inspect_llm_cache(models_dir: &Path, raw_url: &str) -> CacheStatus {
    let Ok(norm) = normalize_hf_url(raw_url) else {
        return CacheStatus::NotDownloaded;
    };
    if let Some(existing) = resolve_existing_llm_path(models_dir, &norm) {
        if let Ok(meta) = fs::metadata(&existing) {
            if meta.len() > 0 {
                return CacheStatus::Ready(meta.len());
            }
        }
    }

    let primary = llm_model_path(models_dir, &norm);
    for candidate in [part_path_for(&primary), primary.with_extension("gguf.part")] {
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.len() > 0 {
                return CacheStatus::Partial(meta.len());
            }
        }
    }

    CacheStatus::NotDownloaded
}

pub fn scan_cache_entries(models_dir: &Path) -> Vec<CacheEntry> {
    let mut entries = Vec::new();
    if !models_dir.is_dir() {
        return entries;
    }

    // ASR directories
    let asr_root = models_dir.join("asr");
    if let Ok(rd) = fs::read_dir(&asr_root) {
        for item in rd.flatten() {
            let p = item.path();
            if p.is_dir() {
                let sz = dir_size_recursive(&p);
                let name = item.file_name().to_string_lossy().into_owned();
                let has_part = fs::read_dir(&p)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|e| is_part_file(&e.path()));
                entries.push(CacheEntry {
                    name,
                    category: "ASR".to_string(),
                    path: p,
                    size_bytes: sz,
                    is_partial: has_part,
                });
            }
        }
    }

    // LLM files under <models_dir>/llm and drop-in *.gguf in <models_dir>
    for (dir, cat) in [(models_dir.join("llm"), "LLM"), (models_dir.to_path_buf(), "LLM")] {
        if let Ok(rd) = fs::read_dir(&dir) {
            for item in rd.flatten() {
                let p = item.path();
                if p.is_file() {
                    let name = item.file_name().to_string_lossy().into_owned();
                    let lower = name.to_ascii_lowercase();
                    if lower.ends_with(".gguf") || lower.ends_with(".part") {
                        let sz = item.metadata().map(|m| m.len()).unwrap_or(0);
                        entries.push(CacheEntry {
                            name,
                            category: cat.to_string(),
                            is_partial: is_part_file(&p),
                            path: p,
                            size_bytes: sz,
                        });
                    }
                }
            }
        }
    }

    // VAD files
    let vad_root = models_dir.join("vad");
    if let Ok(rd) = fs::read_dir(&vad_root) {
        for item in rd.flatten() {
            let p = item.path();
            if p.is_file() {
                let sz = item.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(CacheEntry {
                    name: item.file_name().to_string_lossy().into_owned(),
                    category: "VAD".to_string(),
                    is_partial: is_part_file(&p),
                    path: p,
                    size_bytes: sz,
                });
            }
        }
    }

    entries.sort_by(|a, b| a.category.cmp(&b.category).then(a.name.cmp(&b.name)));
    entries
}

pub fn delete_cache_entry(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("Failed to remove directory {}", path.display()))?;
    } else if path.is_file() {
        fs::remove_file(path)
            .with_context(|| format!("Failed to remove file {}", path.display()))?;
    }
    Ok(())
}

pub fn clean_partial_files(models_dir: &Path) -> Result<u64> {
    let mut freed = 0u64;
    if !models_dir.is_dir() {
        return Ok(0);
    }
    fn walk(dir: &Path, freed: &mut u64) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                walk(&p, freed)?;
            } else if is_part_file(&p) {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                fs::remove_file(&p)?;
                *freed = freed.saturating_add(sz);
            }
        }
        Ok(())
    }
    walk(models_dir, &mut freed)?;
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_asr_file_selector_prefers_int8_variants() {
        let entries = vec![
            HfTreeEntry {
                path: "model.onnx".to_string(),
                size: 900,
                entry_type: "file".to_string(),
            },
            HfTreeEntry {
                path: "model.int8.onnx".to_string(),
                size: 240,
                entry_type: "file".to_string(),
            },
            HfTreeEntry {
                path: "tokens.txt".to_string(),
                size: 12,
                entry_type: "file".to_string(),
            },
            HfTreeEntry {
                path: "README.md".to_string(),
                size: 5,
                entry_type: "file".to_string(),
            },
        ];
        let picked = select_custom_asr_files(&entries);
        let names: Vec<&str> = picked.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["model.int8.onnx", "tokens.txt"]);
    }

    #[test]
    fn cache_inspector_detects_missing_partial_and_ready_states() {
        let tmp = std::env::temp_dir().join(format!(
            "translate_live_cache_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        assert_eq!(
            inspect_asr_cache(&tmp, crate::config::DEFAULT_ASR_URL),
            CacheStatus::NotDownloaded
        );
        assert_eq!(
            inspect_llm_cache(&tmp, crate::config::DEFAULT_LLM_URL),
            CacheStatus::NotDownloaded
        );

        let asr_dir = asr_model_dir(&tmp, crate::config::DEFAULT_ASR_URL);
        fs::create_dir_all(&asr_dir).unwrap();
        fs::write(asr_dir.join("model.int8.onnx.part"), b"partial").unwrap();
        assert_eq!(
            inspect_asr_cache(&tmp, crate::config::DEFAULT_ASR_URL),
            CacheStatus::Partial(7)
        );

        fs::remove_file(asr_dir.join("model.int8.onnx.part")).unwrap();
        fs::write(asr_dir.join("model.int8.onnx"), vec![1u8; 64]).unwrap();
        fs::write(asr_dir.join("tokens.txt"), b"tok").unwrap();
        assert_eq!(
            inspect_asr_cache(&tmp, crate::config::DEFAULT_ASR_URL),
            CacheStatus::Ready(67)
        );

        let llm_path = llm_model_path(&tmp, crate::config::DEFAULT_LLM_URL);
        fs::create_dir_all(llm_path.parent().unwrap()).unwrap();
        fs::write(part_path_for(&llm_path), b"GGUF123").unwrap();
        assert_eq!(
            inspect_llm_cache(&tmp, crate::config::DEFAULT_LLM_URL),
            CacheStatus::Partial(7)
        );
        fs::rename(part_path_for(&llm_path), &llm_path).unwrap();
        assert_eq!(
            inspect_llm_cache(&tmp, crate::config::DEFAULT_LLM_URL),
            CacheStatus::Ready(7)
        );

        let entries = scan_cache_entries(&tmp);
        assert_eq!(entries.len(), 2);

        let _ = fs::remove_dir_all(&tmp);
    }
}
