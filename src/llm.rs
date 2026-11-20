use anyhow::{anyhow, bail, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{language_prompt_name, ComputeBackend};

const N_CTX: u32 = 4096;
const N_BATCH: usize = 1024;

#[derive(Debug, Clone)]
pub struct LoadInfo {
    pub backend_desc: String,
    pub model_name: String,
}

#[derive(Debug, Clone)]
pub struct TranslationResult {
    pub utterance_id: u64,
    pub original: String,
    pub translation: String,
    pub confirmed_prefix_bytes: usize,
    pub elapsed_ms: u128,
    pub tokens_per_sec: f32,
    pub is_final: bool,
}

#[derive(Debug, Clone)]
pub struct TranslateRequest {
    pub utterance_id: u64,
    pub text: String,
    pub source_lang: String,
    pub target_lang: String,
    pub context_pairs: usize,
    pub forced_prefix: Option<String>,
    pub is_final: bool,
}

enum LlmCmd {
    Load {
        path: PathBuf,
        backend_pref: ComputeBackend,
        reply: Sender<Result<LoadInfo>>,
    },
    Unload {
        reply: Sender<()>,
    },
    Translate {
        req: TranslateRequest,
        on_partial: Option<Arc<dyn Fn(String) + Send + Sync>>,
        reply: Sender<Result<TranslationResult>>,
    },
    RecordContext {
        src: String,
        tl: String,
        context_pairs: usize,
    },
    ClearContext,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptStyle {
    QwenNoThink,
    EmbeddedTemplate,
    ChatMlFallback,
}

pub fn build_translation_system_prompt(source_lang: &str, target_lang: &str) -> String {
    let src_desc = if source_lang.eq_ignore_ascii_case("auto") {
        "the spoken source language".to_string()
    } else {
        language_prompt_name(source_lang).to_string()
    };
    let tgt_desc = language_prompt_name(target_lang);
    format!(
        "You are a fast, uncensored live stream subtitle translator. Translate ONLY the current {src_desc} line into ONE natural, idiomatic {tgt_desc} subtitle line. Use any provided recent context strictly for pronouns, names, and continuity — never repeat or re-translate context lines. Preserve slang, tone, humor, and profanity faithfully. If a speaker repeats a word excitedly, keep it natural and concise (at most 2-3 repetitions, never loop). Output ONLY the single-line {tgt_desc} translation with no explanations, quotes, romaji, brackets, or extra lines."
    )
}

pub struct LlmHandle {
    tx: Sender<LlmCmd>,
    join: Option<JoinHandle<()>>,
}

impl LlmHandle {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("llm-worker".to_string())
            .spawn(move || worker_loop(rx))
            .expect("Failed to spawn LLM worker thread");
        Self {
            tx,
            join: Some(join),
        }
    }

    pub fn load(&self, path: PathBuf, backend_pref: ComputeBackend) -> Result<LoadInfo> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(LlmCmd::Load {
                path,
                backend_pref,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("LLM worker thread stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("LLM worker dropped load response"))?
    }

    pub fn unload(&self) {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(LlmCmd::Unload { reply: reply_tx }).is_ok() {
            let _ = reply_rx.recv();
        }
    }

    pub fn clear_context(&self) {
        let _ = self.tx.send(LlmCmd::ClearContext);
    }

    pub fn record_context(&self, src: String, tl: String, context_pairs: usize) {
        let _ = self.tx.send(LlmCmd::RecordContext {
            src,
            tl,
            context_pairs,
        });
    }

    pub fn translate(&self, req: TranslateRequest) -> Result<TranslationResult> {
        self.translate_streaming(req, None)
    }

    pub fn translate_streaming(
        &self,
        req: TranslateRequest,
        on_partial: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Result<TranslationResult> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(LlmCmd::Translate {
                req,
                on_partial,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("LLM worker thread stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("LLM worker dropped translate response"))?
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(LlmCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for LlmHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct CoordQueue {
    finals: VecDeque<TranslateRequest>,
    latest_interim: Option<TranslateRequest>,
    stop: bool,
}

/// Coordinates translation requests so final segments are never dropped while
/// rapid interim ASR updates coalesce (latest-wins), LocalAgreement locks agreed
/// leading words across interim hypotheses to eliminate re-translation flicker,
/// identical interim->final transcripts promote instantly, and live trailing tokens stream.
pub struct TranslationCoordinator {
    queue: Arc<(Mutex<CoordQueue>, Condvar)>,
    join: Option<JoinHandle<()>>,
}

impl TranslationCoordinator {
    pub fn start(
        llm: Arc<LlmHandle>,
        on_partial: Arc<dyn Fn(u64, String, String, usize) + Send + Sync>,
        on_result: Arc<dyn Fn(Result<TranslationResult>) + Send + Sync>,
    ) -> Self {
        let queue = Arc::new((Mutex::new(CoordQueue::default()), Condvar::new()));
        let q_w = Arc::clone(&queue);
        let join = thread::Builder::new()
            .name("tl-coordinator".to_string())
            .spawn(move || {
                let (lock, cvar) = &*q_w;
                let mut utt_id: u64 = 0;
                let mut last_norm_orig = String::new();
                let mut last_tl = String::new();
                let mut locked_prefix = String::new();

                loop {
                    let mut req = {
                        let mut guard = lock.lock().unwrap();
                        loop {
                            if guard.stop {
                                return;
                            }
                            if let Some(f) = guard.finals.pop_front() {
                                break f;
                            }
                            if let Some(i) = guard.latest_interim.take() {
                                break i;
                            }
                            guard = cvar.wait(guard).unwrap();
                        }
                    };

                    if req.utterance_id != utt_id {
                        utt_id = req.utterance_id;
                        last_norm_orig.clear();
                        last_tl.clear();
                        locked_prefix.clear();
                    }

                    let norm_orig = normalize_dedup_key(&req.text);

                    if req.is_final {
                        if last_norm_orig == norm_orig && !last_tl.trim().is_empty() {
                            let cached_tl = last_tl.clone();
                            let conf_len = cached_tl.len();
                            llm.record_context(
                                req.text.clone(),
                                cached_tl.clone(),
                                req.context_pairs,
                            );
                            on_result(Ok(TranslationResult {
                                utterance_id: req.utterance_id,
                                original: req.text,
                                translation: cached_tl,
                                confirmed_prefix_bytes: conf_len,
                                elapsed_ms: 0,
                                tokens_per_sec: 0.0,
                                is_final: true,
                            }));
                            last_norm_orig.clear();
                            last_tl.clear();
                            locked_prefix.clear();
                            continue;
                        }
                    } else if last_norm_orig == norm_orig {
                        continue;
                    }

                    if req.forced_prefix.is_none() {
                        req.forced_prefix = prefill_prefix_for_prompt(&locked_prefix);
                    }

                    let uid = req.utterance_id;
                    let orig_for_cb = req.text.clone();
                    let locked_for_cb = locked_prefix.clone();
                    let cb_ref = Arc::clone(&on_partial);
                    let partial_cb: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |partial| {
                        let conf = clamp_confirmed_bytes(&partial, &locked_for_cb);
                        cb_ref(uid, orig_for_cb.clone(), partial, conf);
                    });

                    let mut res = llm.translate_streaming(req, Some(partial_cb));
                    if let Ok(ref mut out) = res {
                        if out.is_final {
                            out.confirmed_prefix_bytes = out.translation.len();
                            last_norm_orig.clear();
                            last_tl.clear();
                            locked_prefix.clear();
                        } else {
                            locked_prefix =
                                compute_stable_prefix(&last_tl, &out.translation, &locked_prefix);
                            out.confirmed_prefix_bytes =
                                clamp_confirmed_bytes(&out.translation, &locked_prefix);
                            last_norm_orig = norm_orig;
                            last_tl = out.translation.clone();
                        }
                    }
                    on_result(res);
                }
            })
            .expect("Failed to spawn translation coordinator");
        Self {
            queue,
            join: Some(join),
        }
    }

    pub fn submit(&self, req: TranslateRequest) {
        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap();
        if guard.stop {
            return;
        }
        if req.is_final {
            guard.latest_interim = None;
            guard.finals.push_back(req);
        } else {
            guard.latest_interim = Some(req);
        }
        cvar.notify_one();
    }

    pub fn stop(&mut self) {
        let (lock, cvar) = &*self.queue;
        {
            let mut guard = lock.lock().unwrap();
            guard.stop = true;
            guard.finals.clear();
            guard.latest_interim = None;
            cvar.notify_all();
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for TranslationCoordinator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(rx: mpsc::Receiver<LlmCmd>) {
    let mut backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(err) => {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    LlmCmd::Load { reply, .. } => {
                        let _ = reply.send(Err(anyhow!("Failed to init llama backend: {err}")));
                    }
                    LlmCmd::Unload { reply } => {
                        let _ = reply.send(());
                    }
                    LlmCmd::Translate { reply, .. } => {
                        let _ = reply.send(Err(anyhow!("Llama backend unavailable: {err}")));
                    }
                    LlmCmd::RecordContext { .. } | LlmCmd::ClearContext => {}
                    LlmCmd::Shutdown => break,
                }
            }
            return;
        }
    };
    backend.void_logs();

    let mut pending_load: Option<(PathBuf, ComputeBackend, Sender<Result<LoadInfo>>)> = None;
    let mut rolling_history: VecDeque<(String, String)> = VecDeque::with_capacity(6);

    loop {
        let (path, backend_pref, reply) = if let Some(item) = pending_load.take() {
            item
        } else {
            match rx.recv() {
                Ok(LlmCmd::Load {
                    path,
                    backend_pref,
                    reply,
                }) => (path, backend_pref, reply),
                Ok(LlmCmd::Unload { reply }) => {
                    rolling_history.clear();
                    let _ = reply.send(());
                    continue;
                }
                Ok(LlmCmd::ClearContext) => {
                    rolling_history.clear();
                    continue;
                }
                Ok(LlmCmd::RecordContext { .. }) => continue,
                Ok(LlmCmd::Translate { reply, .. }) => {
                    let _ = reply.send(Err(anyhow!("Translation model is not loaded")));
                    continue;
                }
                Ok(LlmCmd::Shutdown) | Err(_) => break,
            }
        };

        let (model, backend_desc) = match load_model_only(&backend, &path, backend_pref) {
            Ok(res) => res,
            Err(err) => {
                let _ = reply.send(Err(err));
                continue;
            }
        };

        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8) as i32)
            .unwrap_or(4);

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_BATCH as u32)
            .with_n_threads(num_threads)
            .with_n_threads_batch(num_threads);

        let mut ctx = match model
            .new_context(&backend, ctx_params)
            .context("Failed to create llama inference context")
        {
            Ok(c) => c,
            Err(err) => {
                let _ = reply.send(Err(err));
                continue;
            }
        };

        let style = detect_prompt_style(&model, &path);

        if style == PromptStyle::QwenNoThink {
            let special_check = model.vocab().tokenize(b"<|im_start|>", false, true);
            if special_check.len() != 1 {
                drop(ctx);
                drop(model);
                let _ = reply.send(Err(anyhow!(
                    "Expected <|im_start|> to tokenize as 1 special token, got {}",
                    special_check.len()
                )));
                continue;
            }
        }

        if let Err(err) = warmup_context(&model, &mut ctx, style) {
            drop(ctx);
            drop(model);
            let _ = reply.send(Err(err));
            continue;
        }

        let model_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model.gguf")
            .to_string();

        rolling_history.clear();
        let _ = reply.send(Ok(LoadInfo {
            backend_desc,
            model_name,
        }));

        let mut should_exit = false;
        loop {
            match rx.recv() {
                Ok(LlmCmd::Translate {
                    req,
                    on_partial,
                    reply,
                }) => {
                    let res = run_translate(
                        &model,
                        &mut ctx,
                        style,
                        &rolling_history,
                        &req,
                        on_partial.as_deref(),
                    );
                    if let Ok(ref out) = res {
                        if req.is_final && !out.translation.trim().is_empty() {
                            push_rolling_pair(
                                &mut rolling_history,
                                req.text,
                                out.translation.clone(),
                                req.context_pairs,
                            );
                        }
                    }
                    let _ = reply.send(res);
                }
                Ok(LlmCmd::RecordContext {
                    src,
                    tl,
                    context_pairs,
                }) => {
                    if !tl.trim().is_empty() {
                        push_rolling_pair(&mut rolling_history, src, tl, context_pairs);
                    }
                }
                Ok(LlmCmd::ClearContext) => {
                    rolling_history.clear();
                }
                Ok(LlmCmd::Unload { reply }) => {
                    drop(ctx);
                    drop(model);
                    rolling_history.clear();
                    let _ = reply.send(());
                    break;
                }
                Ok(LlmCmd::Load {
                    path: next_path,
                    backend_pref: next_pref,
                    reply: next_reply,
                }) => {
                    drop(ctx);
                    drop(model);
                    rolling_history.clear();
                    pending_load = Some((next_path, next_pref, next_reply));
                    break;
                }
                Ok(LlmCmd::Shutdown) | Err(_) => {
                    drop(ctx);
                    drop(model);
                    should_exit = true;
                    break;
                }
            }
        }
        if should_exit {
            break;
        }
    }
}

pub fn normalize_dedup_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub fn push_rolling_pair(
    history: &mut VecDeque<(String, String)>,
    src: String,
    tl: String,
    max_pairs: usize,
) {
    if max_pairs == 0 {
        history.clear();
        return;
    }
    let next_src = normalize_dedup_key(&src);
    let next_tl = normalize_dedup_key(&tl);
    if let Some((last_src, last_tl)) = history.back() {
        let prev_src = normalize_dedup_key(last_src);
        let prev_tl = normalize_dedup_key(last_tl);
        if (!next_src.is_empty() && next_src == prev_src)
            || (!next_tl.is_empty() && next_tl == prev_tl)
        {
            return;
        }
    }
    history.push_back((src, tl));
    while history.len() > max_pairs {
        history.pop_front();
    }
}

fn load_model_only(
    backend: &LlamaBackend,
    path: &Path,
    pref: ComputeBackend,
) -> Result<(LlamaModel, String)> {
    if !path.is_file() {
        bail!("Model file does not exist: {}", path.display());
    }

    if pref == ComputeBackend::VulkanGpu {
        if let Ok(m) = LlamaModel::load_from_file(
            backend,
            path,
            &LlamaModelParams::default().with_n_gpu_layers(u32::MAX),
        ) {
            return Ok((m, "Vulkan GPU".to_string()));
        }
    }

    let m = LlamaModel::load_from_file(
        backend,
        path,
        &LlamaModelParams::default().with_n_gpu_layers(0),
    )
    .with_context(|| format!("Failed to load model from {}", path.display()))?;
    Ok((m, "CPU".to_string()))
}

fn detect_prompt_style(model: &LlamaModel, path: &Path) -> PromptStyle {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if let Ok(tmpl) = model.chat_template(None) {
        let tmpl_str = tmpl.to_str().unwrap_or("");
        if tmpl_str.contains("<|im_start|>")
            && (tmpl_str.contains("<think>") || fname.contains("qwen3"))
        {
            return PromptStyle::QwenNoThink;
        }
        return PromptStyle::EmbeddedTemplate;
    }

    if fname.contains("qwen3") {
        PromptStyle::QwenNoThink
    } else {
        PromptStyle::ChatMlFallback
    }
}

fn warmup_context(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
) -> Result<()> {
    ctx.clear_kv_cache();
    let vocab = model.vocab();
    let empty_hist = VecDeque::new();
    let (probe, add_bos) = build_prompt(
        model,
        style,
        "You are a translator.",
        &empty_hist,
        0,
        "こんにちは",
        None,
    )?;
    let tokens = vocab.tokenize(probe.as_bytes(), add_bos, true);
    if !tokens.is_empty() {
        let mut batch = LlamaBatch::new(tokens.len().max(8), 1);
        let last = tokens.len() - 1;
        for (i, &tok) in tokens.iter().enumerate() {
            batch.add(tok, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch).context("Warmup prefill failed")?;

        let mut sampler = LlamaSampler::greedy();
        let mut n_cur = tokens.len() as i32;
        for _ in 0..3 {
            let token = sampler.sample(ctx, batch.n_tokens() - 1);
            if vocab.is_eog(token) {
                break;
            }
            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            n_cur += 1;
            ctx.decode(&mut batch).context("Warmup decode failed")?;
        }
    }
    ctx.clear_kv_cache();
    Ok(())
}

fn word_spans_with_keys(s: &str) -> Vec<(usize, usize, String)> {
    let mut spans = Vec::new();
    let mut in_word = false;
    let mut start = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if in_word {
                spans.push((start, idx, normalize_dedup_key(&s[start..idx])));
                in_word = false;
            }
        } else if !in_word {
            start = idx;
            in_word = true;
        }
    }
    if in_word {
        spans.push((start, s.len(), normalize_dedup_key(&s[start..])));
    }
    spans
}

pub fn compute_stable_prefix(prev_tl: &str, curr_tl: &str, existing_locked: &str) -> String {
    let prev_words = word_spans_with_keys(prev_tl);
    let curr_words = word_spans_with_keys(curr_tl);
    let mut matched = 0usize;
    for (p, c) in prev_words.iter().zip(curr_words.iter()) {
        if !p.2.is_empty() && p.2 == c.2 {
            matched += 1;
        } else {
            break;
        }
    }
    let lock_count = if matched == curr_words.len() {
        matched.saturating_sub(1)
    } else {
        matched
    };
    let candidate = if lock_count >= 2 {
        curr_tl[..curr_words[lock_count - 1].1]
            .trim_end()
            .to_string()
    } else {
        String::new()
    };
    if !existing_locked.is_empty() && candidate.len() < existing_locked.len() {
        existing_locked.to_string()
    } else {
        candidate
    }
}

pub fn prefill_prefix_for_prompt(locked: &str) -> Option<String> {
    let mut s = locked.trim();
    while s.ends_with(|c: char| matches!(c, '.' | '!' | '?' | '…' | '。' | '！' | '？')) {
        if let Some((idx, _)) = s.char_indices().next_back() {
            s = s[..idx].trim_end();
        } else {
            break;
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

pub fn clamp_confirmed_bytes(translation: &str, locked_prefix: &str) -> usize {
    if locked_prefix.trim().is_empty() || translation.is_empty() {
        return 0;
    }
    let lock_words = word_spans_with_keys(locked_prefix);
    let tl_words = word_spans_with_keys(translation);
    let mut matched = 0usize;
    for (lw, tw) in lock_words.iter().zip(tl_words.iter()) {
        if !lw.2.is_empty() && lw.2 == tw.2 {
            matched += 1;
        } else {
            break;
        }
    }
    if matched == 0 {
        0
    } else {
        tl_words[matched - 1].1.min(translation.len())
    }
}

fn build_prompt(
    model: &LlamaModel,
    style: PromptStyle,
    system_prompt: &str,
    history: &VecDeque<(String, String)>,
    max_pairs: usize,
    user_text: &str,
    forced_prefix: Option<&str>,
) -> Result<(String, bool)> {
    let keep = max_pairs.min(history.len());
    let start_idx = history.len().saturating_sub(keep);

    let user_msg = if keep == 0 {
        user_text.to_string()
    } else {
        let mut s =
            String::from("Recent context (reference only — DO NOT translate or repeat):\n");
        for (src, tl) in history.iter().skip(start_idx) {
            s.push_str(&format!("- {src} -> {tl}\n"));
        }
        s.push_str("\nTranslate ONLY this current line:\n");
        s.push_str(user_text);
        s
    };

    let prefix_str = forced_prefix.unwrap_or("");

    match style {
        PromptStyle::QwenNoThink => {
            let prompt = format!(
                "<|im_start|>system\n{system_prompt}<|im_end|>\n<|im_start|>user\n{user_msg}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n{prefix_str}"
            );
            Ok((prompt, false))
        }
        PromptStyle::EmbeddedTemplate => {
            let tmpl = model
                .chat_template(None)
                .map_err(|e| anyhow!("Missing chat template: {e}"))?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), system_prompt.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_msg)?,
            ];
            let mut prompt = model
                .apply_chat_template(&tmpl, &msgs, true)
                .context("Failed to apply model chat template")?;
            if prompt.ends_with("<think>\n") {
                prompt.push_str("\n</think>\n\n");
            } else if prompt.ends_with("<think>") {
                prompt.push_str("\n\n</think>\n\n");
            }
            if !prefix_str.is_empty() {
                prompt.push_str(prefix_str);
            }
            Ok((prompt, model.vocab().should_add_bos()))
        }
        PromptStyle::ChatMlFallback => {
            let fallback = LlamaChatTemplate::new("chatml")?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), system_prompt.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_msg)?,
            ];
            let mut prompt = model
                .apply_chat_template(&fallback, &msgs, true)
                .context("Failed to apply ChatML fallback template")?;
            if !prefix_str.is_empty() {
                prompt.push_str(prefix_str);
            }
            Ok((prompt, model.vocab().should_add_bos()))
        }
    }
}

fn is_tl_terminal_punct(c: char) -> bool {
    matches!(c, '!' | '?' | '.' | '…' | '！' | '？' | '。')
}

fn carry_terminal_punct(kept_last: &mut String, dropped: &str) {
    if let Some(end_p) = dropped.chars().last().filter(|&c| is_tl_terminal_punct(c)) {
        if !kept_last.chars().last().is_some_and(is_tl_terminal_punct) {
            while kept_last
                .chars()
                .last()
                .is_some_and(|c| matches!(c, ',' | '、' | ';' | ':'))
            {
                kept_last.pop();
            }
            kept_last.push(end_p);
        }
    }
}

fn dedup_consecutive_clauses(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut segments: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut start = 0usize;
    while i < chars.len() {
        if matches!(
            chars[i],
            '.' | '!' | '?' | '。' | '！' | '？' | ';' | '…'
        ) {
            while i + 1 < chars.len()
                && matches!(
                    chars[i + 1],
                    '.' | '!' | '?' | '。' | '！' | '？' | ';' | '…'
                )
            {
                i += 1;
            }
            let seg: String = chars[start..=i].iter().collect();
            let trimmed = seg.trim();
            if !trimmed.is_empty() {
                segments.push(trimmed.to_string());
            }
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    if start < chars.len() {
        let seg: String = chars[start..].iter().collect();
        let trimmed = seg.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
    }
    if segments.len() <= 1 {
        return text.trim().to_string();
    }

    let mut out: Vec<String> = Vec::with_capacity(segments.len());
    let mut prev_key = String::new();
    let mut prev_count = 0usize;
    for seg in segments {
        let key = normalize_dedup_key(&seg);
        let word_count = seg.split_whitespace().count();
        if !key.is_empty() && key == prev_key {
            prev_count += 1;
            let max_allowed = if word_count >= 3 || key.chars().count() >= 10 {
                1
            } else {
                2
            };
            if prev_count <= max_allowed {
                out.push(seg);
            }
        } else {
            prev_key = key;
            prev_count = 1;
            out.push(seg);
        }
    }
    out.join(" ")
}

fn dedup_repeated_phrases(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 4 {
        return text.to_string();
    }
    let keys: Vec<String> = words.iter().map(|w| normalize_dedup_key(w)).collect();
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0usize;
    while i < words.len() {
        let max_n = ((words.len() - i) / 2).min(16);
        let mut matched: Option<(usize, usize)> = None; // (n, end_idx)
        for n in 2..=max_n {
            let pat = &keys[i..i + n];
            if pat.iter().any(|k| k.is_empty()) || pat.iter().all(|k| k == &pat[0]) {
                continue;
            }
            let mut count = 1usize;
            let mut cursor = i + n;
            while cursor + n <= words.len() && &keys[cursor..cursor + n] == pat {
                count += 1;
                cursor += n;
            }
            if count >= 2 {
                let keep_copies = if n >= 3 { 1 } else { 2.min(count) };
                for k_idx in 0..keep_copies {
                    let base = i + k_idx * n;
                    for j in 0..n {
                        out.push(words[base + j].to_string());
                    }
                }
                if count > keep_copies {
                    if let Some(last_kept) = out.last_mut() {
                        carry_terminal_punct(last_kept, words[cursor - 1]);
                    }
                }
                matched = Some((n, cursor));
                break;
            }
        }
        if let Some((_, next_i)) = matched {
            i = next_i;
        } else {
            out.push(words[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

fn cap_hyphen_repeats_in_token(tok: &str, max_run: usize) -> String {
    if !tok.contains('-') {
        return tok.to_string();
    }
    let lead_end = tok
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
        .unwrap_or(tok.len());
    let prefix = &tok[..lead_end];
    let rest = &tok[lead_end..];
    let core_end = rest
        .char_indices()
        .rfind(|(_, c)| c.is_alphanumeric())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let core = &rest[..core_end];
    let suffix = &rest[core_end..];
    if !core.contains('-') {
        return tok.to_string();
    }
    let parts: Vec<&str> = core.split('-').collect();
    if parts.len() <= max_run {
        return tok.to_string();
    }
    let mut kept: Vec<&str> = Vec::with_capacity(parts.len());
    let mut prev_k = String::new();
    let mut run = 0usize;
    for part in parts {
        let k = normalize_dedup_key(part);
        if !k.is_empty() && k == prev_k {
            run += 1;
        } else {
            prev_k = k;
            run = 1;
        }
        if run <= max_run {
            kept.push(part);
        }
    }
    format!("{prefix}{}{suffix}", kept.join("-"))
}

fn dedup_consecutive_words(text: &str, max_run: usize) -> String {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|t| cap_hyphen_repeats_in_token(t, max_run))
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut prev_k = String::new();
    let mut run = 0usize;
    for tok in tokens {
        let k = normalize_dedup_key(&tok);
        if !k.is_empty() && k == prev_k {
            run += 1;
        } else {
            prev_k = k;
            run = 1;
        }
        if run <= max_run {
            out.push(tok);
        } else if let Some(last) = out.last_mut() {
            carry_terminal_punct(last, &tok);
        }
    }
    out.join(" ")
}

pub fn collapse_translation_repetitions(text: &str) -> String {
    let s1 = dedup_consecutive_clauses(text.trim());
    let s2 = dedup_repeated_phrases(&s1);
    dedup_consecutive_words(&s2, 3)
}

fn has_runaway_repetition(visible: &str) -> bool {
    let words: Vec<String> = visible
        .split_whitespace()
        .map(normalize_dedup_key)
        .filter(|w| !w.is_empty())
        .collect();
    let len = words.len();
    if len >= 4 && words[len - 4..].iter().all(|w| w == &words[len - 1]) {
        return true;
    }
    let max_n = (len / 2).min(12);
    for n in 3..=max_n {
        if words[len - 2 * n..len - n] == words[len - n..len] {
            return true;
        }
    }
    if len >= 6
        && words[len - 6..len - 4] == words[len - 4..len - 2]
        && words[len - 4..len - 2] == words[len - 2..len]
    {
        return true;
    }
    false
}

pub fn clean_translation_output(raw: &str) -> String {
    let mut s = if let Some(idx) = raw.rfind("</think>") {
        &raw[idx + "</think>".len()..]
    } else {
        raw
    };

    for stop in ["<|im_end|>", "<|endoftext|>", "<|eot_id|>"] {
        if let Some(idx) = s.find(stop) {
            s = &s[..idx];
        }
    }

    let mut out = s.trim().to_string();

    if out.starts_with("```") && out.ends_with("```") && out.len() >= 6 {
        let inner = &out[3..out.len() - 3];
        let inner = if let Some(nl) = inner.find('\n') {
            let first = inner[..nl].trim();
            if first.len() <= 20 && !first.contains(' ') {
                &inner[nl + 1..]
            } else {
                inner
            }
        } else {
            inner
        };
        out = inner.trim().to_string();
    }

    if let Some(first_line) = out.lines().map(str::trim).find(|l| !l.is_empty()) {
        out = first_line.to_string();
    } else {
        out.clear();
    }

    if out.starts_with('"') && out.ends_with('"') && out.len() >= 2 {
        let inner = &out[1..out.len() - 1];
        if !inner.contains('"') {
            out = inner.trim().to_string();
        }
    }

    collapse_translation_repetitions(&out)
}

fn run_translate_inner(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
    history: &VecDeque<(String, String)>,
    req: &TranslateRequest,
    prefill: Option<&str>,
    on_partial: Option<&(dyn Fn(String) + Send + Sync)>,
) -> Result<(TranslationResult, bool)> {
    let original = req.text.trim().to_string();
    let started = Instant::now();
    let system_prompt = build_translation_system_prompt(&req.source_lang, &req.target_lang);
    let vocab = model.vocab();
    let (prompt, add_bos) = build_prompt(
        model,
        style,
        &system_prompt,
        history,
        req.context_pairs,
        &original,
        prefill,
    )?;
    let prompt_tokens = vocab.tokenize(prompt.as_bytes(), add_bos, true);

    if prompt_tokens.len() + 64 >= N_CTX as usize {
        bail!("Prompt exceeded context limit ({} tokens)", prompt_tokens.len());
    }

    let input_toks = vocab.tokenize(original.as_bytes(), false, false).len();
    let remaining = (N_CTX as usize).saturating_sub(prompt_tokens.len());
    let max_new_tokens = ((input_toks * 3 + 12).max(28)).min(remaining).min(220);

    ctx.clear_kv_cache();

    let mut batch = LlamaBatch::new(N_BATCH, 1);
    let total_prompt = prompt_tokens.len();
    for (chunk_idx, chunk) in prompt_tokens.chunks(N_BATCH).enumerate() {
        batch.clear();
        let base_pos = chunk_idx * N_BATCH;
        for (i, &tok) in chunk.iter().enumerate() {
            let pos = base_pos + i;
            let is_last = pos + 1 == total_prompt;
            batch.add(tok, pos as i32, &[0], is_last)?;
        }
        ctx.decode(&mut batch)
            .context("Failed to decode prompt batch")?;
    }

    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::penalties(model.n_vocab(), 64, 1.18, 0.25, 0.15),
        LlamaSampler::dry(model, 0.8, 1.75, 2, 64, ["\n", ":", "\"", "*"]),
        LlamaSampler::greedy(),
    ]);
    if let Some(p) = prefill {
        let prefill_toks = vocab.tokenize(p.as_bytes(), false, false);
        sampler.accept_many(&prefill_toks);
    }

    let mut n_cur = total_prompt as i32;
    let mut output_bytes: Vec<u8> = Vec::with_capacity(original.len() * 2 + 32);
    let mut generated_tokens = 0usize;
    let mut last_stream_emit = Instant::now();
    let mut last_streamed = String::new();

    for _ in 0..max_new_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        if vocab.is_eog(token) {
            break;
        }
        sampler.accept(token);
        generated_tokens += 1;
        vocab.token_to_piece_into(token, &mut output_bytes, false, None);

        let lossy = String::from_utf8_lossy(&output_bytes);
        if lossy.ends_with("<|im_end|>") || lossy.ends_with("<|endoftext|>") {
            break;
        }

        let in_think = lossy.contains("<think>") && !lossy.contains("</think>");
        if !in_think {
            let visible = if let Some(idx) = lossy.rfind("</think>") {
                &lossy[idx + "</think>".len()..]
            } else {
                &lossy
            };
            let full_visible = match prefill {
                Some(p) => format!("{p}{visible}"),
                None => visible.to_string(),
            };
            if visible.contains('\n') && !full_visible.trim().is_empty() {
                break;
            }
            if has_runaway_repetition(&full_visible) {
                break;
            }
            // Stream progressive tail updates smoothly when a stable prefix is locked
            if prefill.is_some() {
                if let Some(cb) = on_partial {
                    if !visible.trim().is_empty()
                        && last_stream_emit.elapsed() >= Duration::from_millis(60)
                    {
                        let cleaned = clean_translation_output(&full_visible);
                        if !cleaned.is_empty() && cleaned != last_streamed {
                            last_streamed = cleaned.clone();
                            last_stream_emit = Instant::now();
                            cb(cleaned);
                        }
                    }
                }
            }
        }

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        n_cur += 1;
        ctx.decode(&mut batch)
            .context("Failed to decode generated token")?;
    }

    let raw_output = String::from_utf8_lossy(&output_bytes);
    let raw_empty = raw_output.trim().is_empty();
    let full_raw = match prefill {
        Some(p) => format!("{p}{raw_output}"),
        None => raw_output.into_owned(),
    };
    let translation = clean_translation_output(&full_raw);
    let confirmed_prefix_bytes = if req.is_final {
        translation.len()
    } else {
        prefill
            .map(|p| clamp_confirmed_bytes(&translation, p))
            .unwrap_or(0)
    };
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_millis();
    let secs = elapsed.as_secs_f32().max(0.001);
    let tokens_per_sec = (generated_tokens as f32) / secs;

    Ok((
        TranslationResult {
            utterance_id: req.utterance_id,
            original,
            translation,
            confirmed_prefix_bytes,
            elapsed_ms,
            tokens_per_sec,
            is_final: req.is_final,
        },
        raw_empty,
    ))
}

fn run_translate(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
    history: &VecDeque<(String, String)>,
    req: &TranslateRequest,
    on_partial: Option<&(dyn Fn(String) + Send + Sync)>,
) -> Result<TranslationResult> {
    let original = req.text.trim();
    if original.is_empty() {
        return Ok(TranslationResult {
            utterance_id: req.utterance_id,
            original: String::new(),
            translation: String::new(),
            confirmed_prefix_bytes: 0,
            elapsed_ms: 0,
            tokens_per_sec: 0.0,
            is_final: req.is_final,
        });
    }

    let prefill = req
        .forced_prefix
        .as_deref()
        .and_then(prefill_prefix_for_prompt);
    let (res, raw_empty) = run_translate_inner(
        model,
        ctx,
        style,
        history,
        req,
        prefill.as_deref(),
        on_partial,
    )?;

    // Safety fallback: if a forced prefix caused immediate EOS on a long finalized sentence, retry fresh
    if req.is_final && prefill.is_some() && raw_empty && original.chars().count() >= 6 {
        let (retry, _) =
            run_translate_inner(model, ctx, style, history, req, None, on_partial)?;
        return Ok(retry);
    }

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_translation_output_strips_think_stops_and_quotes() {
        let raw = "<think>\nthinking...\n</think>\n\n\"What the hell are you doing?\"<|im_end|>";
        assert_eq!(
            clean_translation_output(raw),
            "What the hell are you doing?"
        );
    }

    #[test]
    fn clean_translation_output_collapses_repetitions() {
        assert_eq!(
            collapse_translation_repetitions(
                "Wait, wait, wait, wait, wait, wait, wait, wait, wait, wait!"
            ),
            "Wait, wait, wait!"
        );
        assert_eq!(
            collapse_translation_repetitions(
                "That boss is way too strong! That boss is way too strong! That boss is way too strong!"
            ),
            "That boss is way too strong!"
        );
        assert_eq!(
            collapse_translation_repetitions("No way! No way! No way! No way!"),
            "No way! No way!"
        );
        assert_eq!(
            clean_translation_output("Hello there\nHello there"),
            "Hello there"
        );
        assert_eq!(
            collapse_translation_repetitions("I think that that is cool"),
            "I think that that is cool"
        );
    }

    #[test]
    fn has_runaway_repetition_detects_loops_only() {
        assert!(has_runaway_repetition("wait wait wait wait"));
        assert!(has_runaway_repetition(
            "this is crazy now this is crazy now"
        ));
        assert!(!has_runaway_repetition("Wait wait what is going on here"));
    }

    #[test]
    fn stable_prefix_locks_agreed_words_monotonically() {
        assert_eq!(compute_stable_prefix("", "My eyes are drying", ""), "");
        assert_eq!(
            compute_stable_prefix("My eyes are drying", "My eyes are drying out now", ""),
            "My eyes are drying"
        );
        assert_eq!(
            compute_stable_prefix("My eyes are drying", "My eyes are drying", ""),
            "My eyes are"
        );
        assert_eq!(
            compute_stable_prefix("Totally different", "Another sentence", "My eyes are"),
            "My eyes are"
        );
    }

    #[test]
    fn prefill_strips_terminal_punct_and_clamps_utf8_boundaries() {
        assert_eq!(
            prefill_prefix_for_prompt("It's drying out."),
            Some("It's drying out".to_string())
        );
        assert_eq!(prefill_prefix_for_prompt("..."), None);
        let tl = "It’s drying out, and I’m blinking!";
        let bytes = clamp_confirmed_bytes(tl, "It’s drying out");
        assert!(tl.is_char_boundary(bytes));
        assert_eq!(&tl[..bytes], "It’s drying out,");
    }

    #[test]
    fn rolling_context_maintains_requested_window_and_deduplicates() {
        let mut hist = VecDeque::new();
        for i in 1..=6 {
            push_rolling_pair(&mut hist, format!("src{i}"), format!("tl{i}"), 4);
        }
        assert_eq!(hist.len(), 4);
        assert_eq!(hist.front().unwrap().0, "src3");
        assert_eq!(hist.back().unwrap().1, "tl6");

        // Duplicate consecutive turn should be ignored
        push_rolling_pair(&mut hist, "src6".to_string(), "tl6!".to_string(), 4);
        assert_eq!(hist.len(), 4);
        assert_eq!(hist.back().unwrap().1, "tl6");

        push_rolling_pair(&mut hist, "x".to_string(), "y".to_string(), 0);
        assert!(hist.is_empty());
    }

    #[test]
    fn system_prompt_targets_requested_languages() {
        let p = build_translation_system_prompt("ja", "en");
        assert!(p.contains("Japanese"));
        assert!(p.contains("English"));
        assert!(p.contains("Output ONLY"));
    }
}
