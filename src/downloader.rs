use anyhow::{anyhow, bail, Context, Result};
use bzip2::read::BzDecoder;
use reqwest::header::{HeaderValue, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tar::Archive;

use crate::config::{
    asr_model_dir, find_asr_preset_by_url, llm_model_path, normalize_asr_url, normalize_hf_url,
    parse_hf_repo_id, resolve_existing_llm_path, silero_vad_path, SILERO_VAD_URL,
};

const MAX_ARCHIVE_UNPACKED_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB safety cap

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
struct HfLfsInfo {
    #[serde(default)]
    oid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HfTreeEntry {
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(rename = "type", default)]
    entry_type: String,
    #[serde(default)]
    lfs: Option<HfLfsInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRangeSpec {
    start: u64,
    end: u64,
    total: Option<u64>,
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

fn is_nonempty_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

fn part_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn etag_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_os_string();
    os.push(".etag");
    PathBuf::from(os)
}

fn is_part_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.ends_with(".part"))
}

fn validate_single_filename(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains(['/', '\\', ':', '\0'])
        || trimmed.chars().any(char::is_control)
    {
        bail!("Invalid or unsafe download filename: {raw:?}");
    }
    let mut comps = Path::new(trimmed).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(trimmed.to_string()),
        _ => bail!("Download filename must be a single normal path segment: {raw:?}"),
    }
}

fn join_safe_child(dir: &Path, filename: &str) -> Result<PathBuf> {
    let safe = validate_single_filename(filename)?;
    let joined = dir.join(&safe);
    if joined.parent() != Some(dir) {
        bail!("Resolved path escaped target directory: {}", joined.display());
    }
    Ok(joined)
}

fn normalize_sha256_hex(raw: &str) -> Option<String> {
    let trimmed = raw.trim().strip_prefix("sha256:").unwrap_or(raw.trim());
    if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(trimmed.to_ascii_lowercase())
    } else {
        None
    }
}

fn verify_file_sha256(path: &Path, expected_hex: &str, cancel: &AtomicBool) -> Result<()> {
    let mut f = File::open(path)
        .with_context(|| format!("Failed to open {} for SHA-256 verification", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected_hex) {
        bail!(
            "SHA-256 checksum mismatch for {} (expected {}, got {})",
            path.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file"),
            expected_hex,
            actual
        );
    }
    Ok(())
}

fn verify_part_file_if_exists(
    part_path: &Path,
    etag_path: &Path,
    validation: FileValidation,
) -> Result<u64> {
    if !part_path.is_file() {
        let _ = fs::remove_file(etag_path);
        return Ok(0);
    }
    let len = fs::metadata(part_path)?.len();
    if len < 4 {
        let _ = fs::remove_file(part_path);
        let _ = fs::remove_file(etag_path);
        return Ok(0);
    }
    if validation == FileValidation::GgufMagic {
        let mut f = File::open(part_path)?;
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic)?;
        if &magic != b"GGUF" {
            let _ = fs::remove_file(part_path);
            let _ = fs::remove_file(etag_path);
            return Ok(0);
        }
    }
    Ok(len)
}

fn parse_content_range(header: Option<&HeaderValue>) -> Option<ContentRangeSpec> {
    let text = header?.to_str().ok()?.trim();
    let rest = text
        .strip_prefix("bytes ")
        .or_else(|| text.strip_prefix("BYTES "))
        .or_else(|| {
            if text.len() >= 6 && text[..6].eq_ignore_ascii_case("bytes ") {
                Some(&text[6..])
            } else {
                None
            }
        })?
        .trim();
    let (range_part, total_part) = rest.split_once('/')?;
    let (start_str, end_str) = range_part.trim().split_once('-')?;
    let start = start_str.trim().parse::<u64>().ok()?;
    let end = end_str.trim().parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    let total_trimmed = total_part.trim();
    let total = if total_trimmed == "*" {
        None
    } else {
        Some(total_trimmed.parse::<u64>().ok()?)
    };
    Some(ContentRangeSpec { start, end, total })
}

fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("translate-live/1.0")
        .https_only(true)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(30))
        .tcp_keepalive(Some(Duration::from_secs(15)))
        .tcp_keepalive_interval(Some(Duration::from_secs(10)))
        .tcp_keepalive_retries(Some(3))
        .build()
        .context("Failed to build HTTP client")
}

fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create downloader runtime")
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype html") || trimmed.starts_with("<html")
}

async fn await_cancellable<T, F>(cancel: &AtomicBool, fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::pin!(fut);
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        tokio::select! {
            biased;
            res = &mut fut => return res,
            _ = ticker.tick() => {}
        }
    }
}

pub fn download_file_resumable(
    client: &Client,
    url: &str,
    dest: &Path,
    validation: FileValidation,
    cancel: &Arc<AtomicBool>,
    on_progress: impl FnMut(u64, Option<u64>, f64),
) -> Result<u64> {
    let rt = build_runtime()?;
    rt.block_on(download_file_resumable_async(
        client,
        url,
        dest,
        validation,
        None,
        cancel,
        on_progress,
    ))
}

async fn download_file_resumable_async(
    client: &Client,
    url: &str,
    dest: &Path,
    validation: FileValidation,
    expected_sha256: Option<&str>,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(u64, Option<u64>, f64),
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create folder {}", parent.display()))?;
    }

    let part_path = part_path_for(dest);
    let etag_path = etag_path_for(dest);

    let mut resp_opt = None;
    let mut resume_len = verify_part_file_if_exists(&part_path, &etag_path, validation)?;
    let mut stored_etag = if resume_len > 0 {
        fs::read_to_string(&etag_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    };

    for attempt in 0..2 {
        let mut req = client.get(url);
        if resume_len > 0 {
            req = req.header(RANGE, format!("bytes={resume_len}-"));
            if let Some(ref etag) = stored_etag {
                req = req.header(IF_RANGE, etag.as_str());
            }
        }

        let resp = await_cancellable(cancel, async {
            req.send()
                .await
                .with_context(|| format!("Failed to connect to {url}"))
        })
        .await?;

        if resume_len > 0 && resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            drop(resp);
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&etag_path);
            resume_len = 0;
            stored_etag = None;
            if attempt == 0 {
                continue;
            }
            bail!("Download failed (HTTP 416) for {url}");
        }

        if resume_len > 0 && resp.status() == StatusCode::PARTIAL_CONTENT {
            let valid_range = parse_content_range(resp.headers().get(CONTENT_RANGE))
                .is_some_and(|cr| cr.start == resume_len);
            let resp_etag = resp
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string());
            let etag_ok = match (&stored_etag, &resp_etag) {
                (Some(prev), Some(curr)) => prev == curr,
                _ => true,
            };
            if !valid_range || !etag_ok {
                drop(resp);
                let _ = fs::remove_file(&part_path);
                let _ = fs::remove_file(&etag_path);
                resume_len = 0;
                stored_etag = None;
                if attempt == 0 {
                    continue;
                }
                bail!("Server returned mismatched byte range while resuming {url}");
            }
        }

        resp_opt = Some(resp);
        break;
    }

    let mut resp = resp_opt.ok_or_else(|| anyhow!("Failed to initiate download for {url}"))?;
    if !resp.status().is_success() {
        bail!("Download failed (HTTP {}) for {url}", resp.status());
    }

    let (mut file, mut downloaded, total, mut checked_header) =
        if resp.status() == StatusCode::PARTIAL_CONTENT && resume_len > 0 {
            let cr = parse_content_range(resp.headers().get(CONTENT_RANGE));
            let total = cr
                .and_then(|c| c.total)
                .or_else(|| resp.content_length().map(|rem| resume_len + rem));
            let mut f = OpenOptions::new()
                .write(true)
                .open(&part_path)
                .with_context(|| format!("Failed to open {}", part_path.display()))?;
            f.set_len(resume_len)?;
            f.seek(SeekFrom::Start(resume_len))?;
            (f, resume_len, total, true)
        } else {
            let _ = fs::remove_file(&etag_path);
            if let Some(etag) = resp
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty() && !s.starts_with("W/"))
            {
                let _ = fs::write(&etag_path, etag);
            }
            let total = resp.content_length();
            let f = File::create(&part_path)
                .with_context(|| format!("Failed to create {}", part_path.display()))?;
            (f, 0u64, total, false)
        };

    let mut header_buf = [0u8; 64];
    let mut header_fill = 0usize;

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

        let maybe_chunk = await_cancellable(cancel, async {
            resp.chunk()
                .await
                .context("Network error while downloading file")
        })
        .await;

        let chunk = match maybe_chunk {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(err) => {
                let _ = file.flush();
                return Err(err);
            }
        };
        if chunk.is_empty() {
            continue;
        }

        if !checked_header {
            let take = (header_buf.len() - header_fill).min(chunk.len());
            header_buf[header_fill..header_fill + take].copy_from_slice(&chunk[..take]);
            header_fill += take;

            match validation {
                FileValidation::GgufMagic if header_fill >= 4 => {
                    if &header_buf[..4] != b"GGUF" {
                        drop(file);
                        let _ = fs::remove_file(&part_path);
                        let _ = fs::remove_file(&etag_path);
                        bail!("URL did not return a valid GGUF model file (check the Hugging Face link)");
                    }
                    checked_header = true;
                }
                FileValidation::RejectHtml if header_fill >= 16 => {
                    if looks_like_html(&header_buf[..header_fill]) {
                        drop(file);
                        let _ = fs::remove_file(&part_path);
                        let _ = fs::remove_file(&etag_path);
                        bail!("URL returned an HTML page instead of a model file ({url})");
                    }
                    checked_header = true;
                }
                _ => {}
            }
        }

        file.write_all(&chunk)
            .context("Failed writing bytes to disk")?;

        downloaded = downloaded.saturating_add(chunk.len() as u64);
        window_bytes = window_bytes.saturating_add(chunk.len() as u64);

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
        let _ = fs::remove_file(&etag_path);
        bail!("Downloaded file was too small to be a valid GGUF model");
    }
    if validation == FileValidation::RejectHtml
        && header_fill > 0
        && looks_like_html(&header_buf[..header_fill])
    {
        drop(file);
        let _ = fs::remove_file(&part_path);
        let _ = fs::remove_file(&etag_path);
        bail!("URL returned an HTML error page instead of model data");
    }

    if let Some(expected) = total {
        if downloaded != expected {
            bail!("Download size mismatch ({downloaded} of {expected} bytes received)");
        }
    }

    file.flush()?;
    file.sync_all()?;
    drop(file);

    if let Some(sha_hex) = expected_sha256.and_then(normalize_sha256_hex) {
        if let Err(err) = verify_file_sha256(&part_path, &sha_hex, cancel) {
            let _ = fs::remove_file(&part_path);
            let _ = fs::remove_file(&etag_path);
            return Err(err);
        }
    }

    on_progress(downloaded, total.or(Some(downloaded)), current_speed);

    let _ = fs::remove_file(&etag_path);
    fs::rename(&part_path, dest).with_context(|| {
        format!(
            "Failed to finalize downloaded file {} -> {}",
            part_path.display(),
            dest.display()
        )
    })?;

    Ok(downloaded)
}

async fn lookup_hf_file_sha256(
    client: &Client,
    url: &str,
    cancel: &AtomicBool,
) -> Option<String> {
    let without_frag = url.trim().split('#').next()?;
    let without_query = without_frag.split('?').next()?;
    let (owner, repo) = parse_hf_repo_id(without_query)?;
    let prefix = format!("https://huggingface.co/{owner}/{repo}/resolve/main/");
    let rel_path = without_query.strip_prefix(&prefix)?;
    if rel_path.is_empty() || rel_path.contains('/') {
        return None;
    }
    let entries = fetch_hf_repo_tree(client, &owner, &repo, cancel)
        .await
        .ok()?;
    entries
        .into_iter()
        .find(|e| e.path == rel_path)
        .and_then(|e| e.lfs)
        .and_then(|l| l.oid)
        .and_then(|oid| normalize_sha256_hex(&oid))
}

pub fn download_llm_model(
    raw_url: &str,
    models_dir: &Path,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let norm = normalize_hf_url(raw_url)?;
    let dest = llm_model_path(models_dir, &norm);
    if is_nonempty_file(&dest) {
        return Ok(dest);
    }
    let client = build_http_client()?;
    let rt = build_runtime()?;
    let label = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("model.gguf")
        .to_string();
    rt.block_on(async {
        let expected_sha = lookup_hf_file_sha256(&client, &norm, &cancel).await;
        download_file_resumable_async(
            &client,
            &norm,
            &dest,
            FileValidation::GgufMagic,
            expected_sha.as_deref(),
            &cancel,
            |downloaded, total, speed_bps| {
                on_progress(DownloadProgress {
                    downloaded,
                    total,
                    speed_bps,
                    label: label.clone(),
                });
            },
        )
        .await
    })?;
    Ok(dest)
}

async fn fetch_hf_repo_tree(
    client: &Client,
    owner: &str,
    repo: &str,
    cancel: &AtomicBool,
) -> Result<Vec<HfTreeEntry>> {
    let api_url = format!("https://huggingface.co/api/models/{owner}/{repo}/tree/main");
    let resp = await_cancellable(cancel, async {
        client
            .get(&api_url)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .with_context(|| format!("Failed querying Hugging Face repo {owner}/{repo}"))
    })
    .await?;
    if !resp.status().is_success() {
        bail!(
            "Hugging Face API returned {} for {owner}/{repo}",
            resp.status()
        );
    }
    let entries: Vec<HfTreeEntry> = await_cancellable(cancel, async {
        resp.json()
            .await
            .with_context(|| format!("Failed parsing file list for {owner}/{repo}"))
    })
    .await?;
    Ok(entries)
}

fn select_custom_asr_files(entries: &[HfTreeEntry]) -> Vec<(String, u64, Option<String>)> {
    let files: Vec<&HfTreeEntry> = entries
        .iter()
        .filter(|e| e.entry_type != "directory")
        .filter(|e| validate_single_filename(&e.path).is_ok())
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
        let sha = f
            .lfs
            .as_ref()
            .and_then(|l| l.oid.as_deref())
            .and_then(normalize_sha256_hex);
        chosen.push((f.path.clone(), f.size, sha));
    }
    chosen
}

fn flatten_single_subdir(dest_dir: &Path) -> Result<()> {
    let entries: Vec<PathBuf> = fs::read_dir(dest_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    if entries.len() == 1 && entries[0].is_dir() {
        let inner = &entries[0];
        let tmp_inner = dest_dir.join(format!(".__flatten_tmp_{}", std::process::id()));
        if tmp_inner.exists() {
            let _ = fs::remove_dir_all(&tmp_inner);
        }
        fs::rename(inner, &tmp_inner)?;
        for child in fs::read_dir(&tmp_inner)?.flatten() {
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
        let _ = fs::remove_dir(&tmp_inner);
    }
    Ok(())
}

fn extract_tar_bz2_safe(archive_path: &Path, staging_dir: &Path, cancel: &AtomicBool) -> Result<()> {
    let f = File::open(archive_path)
        .with_context(|| format!("Failed to open {}", archive_path.display()))?;
    let dec = BzDecoder::new(f);
    let mut ar = Archive::new(dec);
    let mut total_unpacked: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    for entry_res in ar.entries().context("Failed reading .tar.bz2 archive entries")? {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        let mut entry = entry_res.context("Failed reading .tar.bz2 entry")?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            bail!("ASR archive contains unsupported link entry");
        }
        if !(entry_type.is_dir() || entry_type.is_file()) {
            continue;
        }

        let raw_path = entry.path().context("Invalid path inside ASR archive")?;
        let mut rel = PathBuf::new();
        for comp in raw_path.components() {
            match comp {
                Component::Normal(os) => {
                    let s = os
                        .to_str()
                        .ok_or_else(|| anyhow!("Non-UTF-8 path in ASR archive"))?;
                    if s.is_empty() || s.contains(['/', '\\', ':', '\0']) || s.chars().any(char::is_control) {
                        bail!("Unsafe filename inside ASR archive: {s:?}");
                    }
                    rel.push(os);
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    bail!("Unsafe path traversal inside ASR archive: {}", raw_path.display());
                }
            }
        }
        if rel.as_os_str().is_empty() {
            continue;
        }

        let out_path = staging_dir.join(&rel);
        if entry_type.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = File::create(&out_path)
            .with_context(|| format!("Failed to create extracted file {}", out_path.display()))?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("Download cancelled");
            }
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total_unpacked = total_unpacked.saturating_add(n as u64);
            if total_unpacked > MAX_ARCHIVE_UNPACKED_BYTES {
                bail!("ASR archive exceeds maximum unpacked size limit");
            }
            out_file.write_all(&buf[..n])?;
        }
        out_file.flush()?;
    }

    drop(ar);
    Ok(())
}

fn move_dir_contents(src_dir: &Path, dst_dir: &Path) -> Result<()> {
    if !dst_dir.exists() {
        fs::rename(src_dir, dst_dir).with_context(|| {
            format!(
                "Failed moving staged ASR dir {} -> {}",
                src_dir.display(),
                dst_dir.display()
            )
        })?;
        return Ok(());
    }
    for item in fs::read_dir(src_dir)? {
        let item = item?;
        let from = item.path();
        let to = dst_dir.join(item.file_name());
        if to.exists() {
            if to.is_dir() {
                fs::remove_dir_all(&to)?;
            } else {
                fs::remove_file(&to)?;
            }
        }
        fs::rename(&from, &to)?;
    }
    let _ = fs::remove_dir_all(src_dir);
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
    let rt = build_runtime()?;
    let dest_dir = asr_model_dir(models_dir, &norm);

    // Check if it's a direct .tar.bz2 archive link
    let lower_no_query = norm
        .split('?')
        .next()
        .unwrap_or(&norm)
        .to_ascii_lowercase();
    if lower_no_query.ends_with(".tar.bz2") {
        let slug_name = dest_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("asr");
        let asr_parent = dest_dir.parent().unwrap_or(models_dir);
        fs::create_dir_all(asr_parent)
            .with_context(|| format!("Failed to create ASR dir {}", asr_parent.display()))?;

        let archive_path = asr_parent.join(format!("{slug_name}.archive.tar.bz2"));
        let staging_dir = asr_parent.join(format!("{slug_name}.staging"));

        rt.block_on(download_file_resumable_async(
            &client,
            &norm,
            &archive_path,
            FileValidation::RejectHtml,
            None,
            &cancel,
            |downloaded, total, speed_bps| {
                on_progress(DownloadProgress {
                    downloaded,
                    total,
                    speed_bps,
                    label: "ASR archive (.tar.bz2)".to_string(),
                });
            },
        ))?;

        on_progress(DownloadProgress {
            downloaded: 0,
            total: None,
            speed_bps: 0.0,
            label: "Extracting ASR archive...".to_string(),
        });

        let _ = fs::remove_dir_all(&staging_dir);
        fs::create_dir_all(&staging_dir)
            .with_context(|| format!("Failed to create staging dir {}", staging_dir.display()))?;

        let extract_res = (|| -> Result<()> {
            extract_tar_bz2_safe(&archive_path, &staging_dir, &cancel)?;
            let _ = fs::remove_file(&archive_path);
            flatten_single_subdir(&staging_dir)?;
            move_dir_contents(&staging_dir, &dest_dir)?;
            Ok(())
        })();
        if let Err(err) = extract_res {
            let _ = fs::remove_dir_all(&staging_dir);
            return Err(err).context("Failed to extract .tar.bz2 ASR archive");
        }

        if ensure_silero_vad {
            rt.block_on(ensure_silero_vad_downloaded_async(
                &client,
                models_dir,
                &cancel,
                &mut on_progress,
            ))?;
        }
        return Ok(dest_dir);
    }

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create ASR dir {}", dest_dir.display()))?;

    // Build file list: (download_url, local_path, known_size, expected_sha256)
    let plan: Vec<(String, PathBuf, Option<u64>, Option<String>)> = rt.block_on(async {
        let mut plan: Vec<(String, PathBuf, Option<u64>, Option<String>)> = Vec::new();
        if let Some(preset) = find_asr_preset_by_url(&norm) {
            let (owner, repo) = parse_hf_repo_id(preset.url)
                .ok_or_else(|| anyhow!("Invalid preset HF URL: {}", preset.url))?;
            let meta_map = fetch_hf_repo_tree(&client, &owner, &repo, &cancel)
                .await
                .ok()
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|e| {
                            let sha = e
                                .lfs
                                .as_ref()
                                .and_then(|l| l.oid.as_deref())
                                .and_then(normalize_sha256_hex);
                            (e.path, (e.size, sha))
                        })
                        .collect::<HashMap<String, (u64, Option<String>)>>()
                })
                .unwrap_or_default();

            for &fname in preset.files {
                let f_dest = join_safe_child(&dest_dir, fname)?;
                let f_url = format!("https://huggingface.co/{owner}/{repo}/resolve/main/{fname}");
                let (known_sz, sha) = meta_map
                    .get(fname)
                    .map(|(sz, s)| (Some(*sz), s.clone()))
                    .unwrap_or((None, None));
                plan.push((f_url, f_dest, known_sz, sha));
            }
        } else if lower_no_query.ends_with(".onnx") {
            let without_frag = norm.split('#').next().unwrap_or(&norm);
            let without_query = without_frag.split('?').next().unwrap_or(without_frag);
            let raw_fname = without_query
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("model.onnx");
            let f_dest = join_safe_child(&dest_dir, raw_fname)?;
            let expected_sha = lookup_hf_file_sha256(&client, &norm, &cancel).await;
            plan.push((norm.clone(), f_dest, None, expected_sha));
        } else if let Some((owner, repo)) = parse_hf_repo_id(&norm) {
            let entries = fetch_hf_repo_tree(&client, &owner, &repo, &cancel).await?;
            let selected = select_custom_asr_files(&entries);
            if selected.is_empty() {
                bail!("No ONNX ASR model files found in {owner}/{repo}");
            }
            for (fname, sz, sha) in selected {
                let safe_name = validate_single_filename(&fname)?;
                let f_dest = join_safe_child(&dest_dir, &safe_name)?;
                let f_url = format!("https://huggingface.co/{owner}/{repo}/resolve/main/{safe_name}");
                plan.push((f_url, f_dest, Some(sz), sha));
            }
        } else {
            bail!("Unsupported ASR URL format. Paste a Hugging Face model repo URL or .tar.bz2 link.");
        }

        let vad_dest = silero_vad_path(models_dir);
        let need_vad = ensure_silero_vad && !is_nonempty_file(&vad_dest);
        let vad_est = if need_vad { Some(1_807_522u64) } else { None };
        if need_vad {
            let vad_sha = lookup_hf_file_sha256(&client, SILERO_VAD_URL, &cancel).await;
            plan.push((SILERO_VAD_URL.to_string(), vad_dest, vad_est, vad_sha));
        }
        Ok::<_, anyhow::Error>(plan)
    })?;

    let known_total: Option<u64> = if plan.iter().all(|(_, _, s, _)| s.is_some()) {
        Some(plan.iter().filter_map(|(_, _, s, _)| *s).sum())
    } else {
        None
    };

    let mut completed_bytes = 0u64;
    for (f_url, f_dest, known_sz, expected_sha) in &plan {
        let fname = f_dest
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();

        if is_nonempty_file(f_dest) {
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
        let received = rt.block_on(download_file_resumable_async(
            &client,
            f_url,
            f_dest,
            FileValidation::RejectHtml,
            expected_sha.as_deref(),
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
        ))?;
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
    let rt = build_runtime()?;
    rt.block_on(ensure_silero_vad_downloaded_async(
        &client,
        models_dir,
        &cancel,
        &mut on_progress,
    ))
}

async fn ensure_silero_vad_downloaded_async(
    client: &Client,
    models_dir: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut impl FnMut(DownloadProgress),
) -> Result<PathBuf> {
    let dest = silero_vad_path(models_dir);
    if is_nonempty_file(&dest) {
        return Ok(dest);
    }
    let expected_sha = lookup_hf_file_sha256(client, SILERO_VAD_URL, cancel).await;
    download_file_resumable_async(
        client,
        SILERO_VAD_URL,
        &dest,
        FileValidation::RejectHtml,
        expected_sha.as_deref(),
        cancel,
        |downloaded, total, speed_bps| {
            on_progress(DownloadProgress {
                downloaded,
                total,
                speed_bps,
                label: "silero_vad.onnx".to_string(),
            });
        },
    )
    .await?;
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
                } else if name.ends_with(".etag") {
                    continue;
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
                let name = item.file_name().to_string_lossy().into_owned();
                if name.to_ascii_lowercase().ends_with(".etag") {
                    continue;
                }
                let sz = item.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(CacheEntry {
                    name,
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
        let _ = fs::remove_file(etag_path_for(path));
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
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(&p, freed)?;
            } else if ft.is_file()
                && (is_part_file(&p) || p.extension().is_some_and(|e| e == "etag"))
            {
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
    fn custom_asr_file_selector_prefers_int8_variants_and_rejects_traversal() {
        let entries = vec![
            HfTreeEntry {
                path: "model.onnx".to_string(),
                size: 900,
                entry_type: "file".to_string(),
                lfs: None,
            },
            HfTreeEntry {
                path: "model.int8.onnx".to_string(),
                size: 240,
                entry_type: "file".to_string(),
                lfs: None,
            },
            HfTreeEntry {
                path: "C:evil.onnx".to_string(),
                size: 100,
                entry_type: "file".to_string(),
                lfs: None,
            },
            HfTreeEntry {
                path: "..\\..\\evil.onnx".to_string(),
                size: 100,
                entry_type: "file".to_string(),
                lfs: None,
            },
            HfTreeEntry {
                path: "tokens.txt".to_string(),
                size: 12,
                entry_type: "file".to_string(),
                lfs: None,
            },
            HfTreeEntry {
                path: "README.md".to_string(),
                size: 5,
                entry_type: "file".to_string(),
                lfs: None,
            },
        ];
        let picked = select_custom_asr_files(&entries);
        let names: Vec<&str> = picked.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["model.int8.onnx", "tokens.txt"]);
    }

    #[test]
    fn validates_single_filenames_and_content_range() {
        assert_eq!(validate_single_filename("model.onnx").unwrap(), "model.onnx");
        assert!(validate_single_filename("..\\..\\evil.onnx").is_err());
        assert!(validate_single_filename("C:evil.onnx").is_err());
        assert!(validate_single_filename("C:\\evil.onnx").is_err());
        assert!(validate_single_filename("a/b.onnx").is_err());
        assert!(validate_single_filename("..").is_err());

        let parsed = parse_content_range(Some(&HeaderValue::from_static("bytes 100-199/500")));
        assert_eq!(
            parsed,
            Some(ContentRangeSpec {
                start: 100,
                end: 199,
                total: Some(500),
            })
        );
        assert!(parse_content_range(Some(&HeaderValue::from_static("bytes 200-100/500"))).is_none());
    }

    #[test]
    fn flatten_single_subdir_handles_same_named_nested_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "translate_live_flatten_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let nested = tmp.join("foo").join("foo");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("inner.txt"), b"ok").unwrap();
        fs::write(tmp.join("foo").join("model.onnx"), b"onnx").unwrap();

        flatten_single_subdir(&tmp).unwrap();
        assert!(tmp.join("model.onnx").is_file());
        assert!(tmp.join("foo").join("inner.txt").is_file());
        let _ = fs::remove_dir_all(&tmp);
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
