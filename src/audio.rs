use anyhow::{anyhow, Context, Result};
use sherpa_onnx::LinearResampler;
use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
pub const DEFAULT_MIC_SENTINEL: &str = "__default__";

#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AudioDeviceList {
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

struct ComGuard(bool);

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

fn init_com_mta() -> Result<ComGuard> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_ok() {
            Ok(ComGuard(true))
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(ComGuard(false))
        } else {
            Err(anyhow!("CoInitializeEx failed: {hr}"))
        }
    }
}

unsafe fn get_device_id(dev: &IMMDevice) -> Result<String> {
    let pwstr = dev.GetId()?;
    let id = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.0 as _));
    Ok(id)
}

unsafe fn get_device_name(dev: &IMMDevice) -> String {
    let Ok(store) = dev.OpenPropertyStore(STGM_READ) else {
        return "Audio Device".to_string();
    };
    let Ok(mut pv) = store.GetValue(&PKEY_Device_FriendlyName) else {
        return "Audio Device".to_string();
    };
    let name = match PropVariantToStringAlloc(&pv) {
        Ok(pwstr) => {
            let s = pwstr.to_string().unwrap_or_else(|_| "Audio Device".to_string());
            CoTaskMemFree(Some(pwstr.0 as _));
            s
        }
        Err(_) => "Audio Device".to_string(),
    };
    let _ = PropVariantClear(&mut pv);
    name
}

unsafe fn enumerate_flow(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Vec<AudioDevice> {
    let default_id = enumerator
        .GetDefaultAudioEndpoint(flow, eConsole)
        .ok()
        .and_then(|d| get_device_id(&d).ok());

    let Ok(collection) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
        return Vec::new();
    };
    let count = collection.GetCount().unwrap_or(0);
    let mut devices = Vec::with_capacity(count as usize);

    for i in 0..count {
        if let Ok(dev) = collection.Item(i) {
            if let Ok(id) = get_device_id(&dev) {
                let name = get_device_name(&dev);
                let is_default = default_id.as_deref() == Some(id.as_str());
                devices.push(AudioDevice {
                    id,
                    name,
                    is_default,
                });
            }
        }
    }
    devices
}

pub fn enumerate_audio_devices() -> Result<AudioDeviceList> {
    let _com = init_com_mta()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("Failed to create MMDeviceEnumerator")?;
        let outputs = enumerate_flow(&enumerator, eRender);
        let inputs = enumerate_flow(&enumerator, eCapture);
        Ok(AudioDeviceList { outputs, inputs })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    Float32,
    Pcm16,
    Pcm24,
    Pcm32,
}

unsafe fn detect_sample_kind(fmt: *const WAVEFORMATEX) -> Result<(SampleKind, u32, usize)> {
    let wfx = &*fmt;
    let tag = wfx.wFormatTag as u32;
    let channels = wfx.nChannels.max(1) as usize;
    let sample_rate = wfx.nSamplesPerSec.max(8000);
    let bits = wfx.wBitsPerSample;

    if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
        return Ok((SampleKind::Float32, sample_rate, channels));
    }
    if tag == WAVE_FORMAT_PCM {
        return match bits {
            16 => Ok((SampleKind::Pcm16, sample_rate, channels)),
            24 => Ok((SampleKind::Pcm24, sample_rate, channels)),
            32 => Ok((SampleKind::Pcm32, sample_rate, channels)),
            _ => Err(anyhow!("Unsupported PCM bit depth: {bits}")),
        };
    }
    if tag == WAVE_FORMAT_EXTENSIBLE {
        let min_cb = size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>();
        let cb_size = wfx.cbSize;
        if (cb_size as usize) < min_cb {
            return Err(anyhow!(
                "Invalid WAVEFORMATEXTENSIBLE cbSize: {cb_size} < {min_cb}"
            ));
        }
        let ext_ptr = fmt as *const WAVEFORMATEXTENSIBLE;
        let sub = std::ptr::addr_of!((*ext_ptr).SubFormat).read_unaligned();
        if sub == SUBTYPE_IEEE_FLOAT && bits == 32 {
            return Ok((SampleKind::Float32, sample_rate, channels));
        }
        if sub == KSDATAFORMAT_SUBTYPE_PCM {
            return match bits {
                16 => Ok((SampleKind::Pcm16, sample_rate, channels)),
                24 => Ok((SampleKind::Pcm24, sample_rate, channels)),
                32 => Ok((SampleKind::Pcm32, sample_rate, channels)),
                _ => Err(anyhow!("Unsupported extensible PCM bit depth: {bits}")),
            };
        }
    }

    Err(anyhow!(
        "Unsupported WASAPI audio format (tag={tag}, bits={bits})"
    ))
}

unsafe fn decode_frames_to_mono(
    data: *const u8,
    num_frames: usize,
    channels: usize,
    kind: SampleKind,
    out: &mut Vec<f32>,
) {
    out.clear();
    let sample_bytes = match kind {
        SampleKind::Float32 | SampleKind::Pcm32 => 4usize,
        SampleKind::Pcm24 => 3usize,
        SampleKind::Pcm16 => 2usize,
    };
    let Some(frame_bytes) = channels.checked_mul(sample_bytes) else {
        return;
    };
    let Some(total_bytes) = num_frames.checked_mul(frame_bytes) else {
        return;
    };
    if data.is_null() || total_bytes == 0 || frame_bytes == 0 {
        return;
    }

    out.reserve(num_frames);
    let inv_ch = 1.0 / (channels as f32);
    let bytes = std::slice::from_raw_parts(data, total_bytes);

    match kind {
        SampleKind::Float32 => {
            for frame in bytes.chunks_exact(frame_bytes) {
                let sum: f32 = frame
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .sum();
                out.push((sum * inv_ch).clamp(-1.0, 1.0));
            }
        }
        SampleKind::Pcm16 => {
            for frame in bytes.chunks_exact(frame_bytes) {
                let sum: f32 = frame
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .sum();
                out.push((sum * inv_ch).clamp(-1.0, 1.0));
            }
        }
        SampleKind::Pcm24 => {
            for frame in bytes.chunks_exact(frame_bytes) {
                let mut sum = 0.0f32;
                for s in frame.chunks_exact(3) {
                    let raw = ((s[0] as i32) << 8 | (s[1] as i32) << 16 | (s[2] as i32) << 24) >> 8;
                    sum += (raw as f32) / 8_388_608.0;
                }
                out.push((sum * inv_ch).clamp(-1.0, 1.0));
            }
        }
        SampleKind::Pcm32 => {
            for frame in bytes.chunks_exact(frame_bytes) {
                let sum: f32 = frame
                    .chunks_exact(4)
                    .map(|b| (i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32) / 2_147_483_648.0)
                    .sum();
                out.push((sum * inv_ch).clamp(-1.0, 1.0));
            }
        }
    }
}

pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt() as f32).clamp(0.0, 1.0)
}

unsafe fn open_device_by_id(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
    device_id: Option<&str>,
) -> Result<IMMDevice> {
    if let Some(id) = device_id.filter(|s| !s.is_empty() && *s != DEFAULT_MIC_SENTINEL) {
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(dev) = enumerator.GetDevice(PCWSTR(wide.as_ptr())) {
            return Ok(dev);
        }
    }
    enumerator
        .GetDefaultAudioEndpoint(flow, eConsole)
        .context("No default Windows audio endpoint available")
}

struct CoTaskMemGuard(*mut core::ffi::c_void);

impl Drop for CoTaskMemGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CoTaskMemFree(Some(self.0));
            }
        }
    }
}

struct WasapiStream {
    capture_client: IAudioCaptureClient,
    audio_client: IAudioClient,
    kind: SampleKind,
    channels: usize,
    sample_rate: u32,
    bytes_per_frame: usize,
    resampler: Option<LinearResampler>,
    _com: ComGuard,
}

impl WasapiStream {
    fn open(flow: EDataFlow, device_id: Option<&str>, loopback: bool) -> Result<Self> {
        let com = init_com_mta()?;
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("Failed to create MMDeviceEnumerator")?;
            let dev = open_device_by_id(&enumerator, flow, device_id)?;
            let audio_client: IAudioClient = dev
                .Activate(CLSCTX_ALL, None)
                .context("Failed to activate IAudioClient")?;

            let fmt_ptr = audio_client
                .GetMixFormat()
                .context("Failed to query WASAPI mix format")?;
            let fmt_guard = CoTaskMemGuard(fmt_ptr as _);
            let (kind, sample_rate, channels) = detect_sample_kind(fmt_ptr)?;
            let bits = (*fmt_ptr).wBitsPerSample as usize;
            let bytes_per_frame = channels * (bits / 8);

            let stream_flags = if loopback {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
            let res = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                200_000, // 20 ms
                0,
                fmt_ptr,
                None,
            );
            drop(fmt_guard);
            res.context("IAudioClient::Initialize failed")?;

            let capture_client: IAudioCaptureClient = audio_client
                .GetService()
                .context("Failed to acquire IAudioCaptureClient")?;
            audio_client
                .Start()
                .context("Failed to start WASAPI capture stream")?;

            let resampler = if sample_rate != 16000 {
                let rs = LinearResampler::create(sample_rate as i32, 16000).ok_or_else(|| {
                    anyhow!("Failed to initialize audio resampler ({sample_rate} Hz -> 16000 Hz)")
                })?;
                Some(rs)
            } else {
                None
            };

            Ok(Self {
                capture_client,
                audio_client,
                kind,
                channels,
                sample_rate,
                bytes_per_frame,
                resampler,
                _com: com,
            })
        }
    }

    fn read_available_16k(&self, scratch_mono: &mut Vec<f32>, out_16k: &mut Vec<f32>) -> Result<bool> {
        out_16k.clear();
        let mut had_packet = false;
        unsafe {
            loop {
                let packet_len = self.capture_client.GetNextPacketSize()?;
                if packet_len == 0 {
                    break;
                }
                had_packet = true;
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                self.capture_client
                    .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)?;

                let n = num_frames as usize;
                if n > 0 {
                    if (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 || data_ptr.is_null() {
                        scratch_mono.clear();
                        scratch_mono.resize(n, 0.0);
                    } else {
                        let _ = self.bytes_per_frame;
                        decode_frames_to_mono(
                            data_ptr,
                            n,
                            self.channels,
                            self.kind,
                            scratch_mono,
                        );
                    }

                    if let Some(ref rs) = self.resampler {
                        let resampled = rs.resample(scratch_mono, false);
                        out_16k.extend_from_slice(&resampled);
                    } else if self.sample_rate == 16000 {
                        out_16k.extend_from_slice(scratch_mono);
                    }
                }

                self.capture_client.ReleaseBuffer(num_frames)?;
            }
        }
        Ok(had_packet)
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
        }
    }
}

pub struct AudioCaptureHandle {
    stop_flag: Arc<AtomicBool>,
    loopback_join: Option<JoinHandle<()>>,
    mic_join: Option<JoinHandle<()>>,
}

impl AudioCaptureHandle {
    pub fn start(
        loopback_device_id: Option<String>,
        mic_device_id: Option<String>,
        rms_bits: Arc<AtomicU32>,
        on_error: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<(Self, Receiver<Vec<f32>>)> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Vec<f32>>();

        let mic_ring = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(16000)));
        let mic_join = if let Some(mic_id) = mic_device_id {
            let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
            let stop_m = Arc::clone(&stop_flag);
            let ring_m = Arc::clone(&mic_ring);
            let join = thread::Builder::new()
                .name("wasapi-mic".to_string())
                .spawn(move || {
                    mic_loop(mic_id, stop_m, ring_m, ready_tx);
                })
                .context("Failed to spawn WASAPI mic thread")?;
            match ready_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    stop_flag.store(true, Ordering::SeqCst);
                    let _ = join.join();
                    return Err(err);
                }
                Err(_) => {
                    stop_flag.store(true, Ordering::SeqCst);
                    let _ = join.join();
                    return Err(anyhow!("Timed out opening WASAPI microphone device"));
                }
            }
            Some(join)
        } else {
            None
        };

        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let stop_l = Arc::clone(&stop_flag);
        let ring_l = Arc::clone(&mic_ring);
        let loopback_join = match thread::Builder::new()
            .name("wasapi-loopback".to_string())
            .spawn(move || {
                loopback_loop(
                    loopback_device_id,
                    stop_l,
                    ring_l,
                    tx,
                    rms_bits,
                    ready_tx,
                    on_error,
                );
            }) {
            Ok(j) => j,
            Err(e) => {
                stop_flag.store(true, Ordering::SeqCst);
                if let Some(mj) = mic_join {
                    let _ = mj.join();
                }
                return Err(e).context("Failed to spawn WASAPI loopback thread");
            }
        };

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = loopback_join.join();
                if let Some(mj) = mic_join {
                    let _ = mj.join();
                }
                return Err(err);
            }
            Err(_) => {
                stop_flag.store(true, Ordering::SeqCst);
                let _ = loopback_join.join();
                if let Some(mj) = mic_join {
                    let _ = mj.join();
                }
                return Err(anyhow!("Timed out opening WASAPI loopback audio device"));
            }
        }

        Ok((
            Self {
                stop_flag,
                loopback_join: Some(loopback_join),
                mic_join,
            },
            rx,
        ))
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(j) = self.loopback_join.take() {
            let _ = j.join();
        }
        if let Some(j) = self.mic_join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for AudioCaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn mic_loop(
    mic_id: String,
    stop_flag: Arc<AtomicBool>,
    mic_ring: Arc<Mutex<VecDeque<f32>>>,
    ready_tx: Sender<Result<()>>,
) {
    let stream = match WasapiStream::open(eCapture, Some(&mic_id), false) {
        Ok(s) => {
            let _ = ready_tx.send(Ok(()));
            s
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let mut scratch = Vec::with_capacity(2048);
    let mut out_16k = Vec::with_capacity(2048);

    while !stop_flag.load(Ordering::Relaxed) {
        match stream.read_available_16k(&mut scratch, &mut out_16k) {
            Ok(true) if !out_16k.is_empty() => {
                let mut ring = mic_ring.lock().unwrap_or_else(|e| e.into_inner());
                if ring.len() > 16000 {
                    let drop_n = ring.len() - 8000;
                    ring.drain(..drop_n);
                }
                ring.extend(out_16k.iter().copied());
            }
            Ok(_) => {
                thread::sleep(Duration::from_millis(8));
            }
            Err(_) => break,
        }
    }
}

fn loopback_loop(
    device_id: Option<String>,
    stop_flag: Arc<AtomicBool>,
    mic_ring: Arc<Mutex<VecDeque<f32>>>,
    tx: Sender<Vec<f32>>,
    rms_bits: Arc<AtomicU32>,
    ready_tx: Sender<Result<()>>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
) {
    let stream = match WasapiStream::open(eRender, device_id.as_deref(), true) {
        Ok(s) => {
            let _ = ready_tx.send(Ok(()));
            s
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };

    let mut scratch = Vec::with_capacity(2048);
    let mut out_16k = Vec::with_capacity(2048);

    while !stop_flag.load(Ordering::Relaxed) {
        match stream.read_available_16k(&mut scratch, &mut out_16k) {
            Ok(had_packet) => {
                // Critical WASAPI loopback silence injection: when playback stops/pauses, Windows
                // emits zero packets, so inject silence sized to actual elapsed sleep to flush VAD
                // and drain optional mic ring at real-time pace.
                let injected_silence = !had_packet || out_16k.is_empty();
                if injected_silence {
                    let before_sleep = Instant::now();
                    thread::sleep(Duration::from_millis(10));
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    let elapsed_secs = before_sleep.elapsed().as_secs_f64();
                    let silence_samples =
                        ((elapsed_secs * 16_000.0).round() as usize).clamp(160, 1600);
                    out_16k.clear();
                    out_16k.resize(silence_samples, 0.0);
                }

                // Mix optional mic ring samples if available.
                {
                    let mut ring = mic_ring.lock().unwrap_or_else(|e| e.into_inner());
                    if !ring.is_empty() {
                        for s in out_16k.iter_mut() {
                            if let Some(m) = ring.pop_front() {
                                *s = (*s + m).clamp(-1.0, 1.0);
                            } else {
                                break;
                            }
                        }
                    }
                }

                let rms = compute_rms(&out_16k);
                rms_bits.store(rms.to_bits(), Ordering::Relaxed);
                if tx.send(out_16k.clone()).is_err() {
                    break;
                }
                if !injected_silence {
                    thread::sleep(Duration::from_millis(4));
                }
            }
            Err(err) => {
                on_error(format!("WASAPI loopback error: {err}"));
                break;
            }
        }
    }
    rms_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_computes_zero_for_silence_and_accurate_sine_level() {
        assert_eq!(compute_rms(&[]), 0.0);
        assert_eq!(compute_rms(&[0.0; 160]), 0.0);
        let peak = compute_rms(&[0.5, -0.5, 0.5, -0.5]);
        assert!((peak - 0.5).abs() < 1e-5);
    }

    #[test]
    fn wasapi_enumeration_runs_without_crashing() {
        let res = enumerate_audio_devices();
        assert!(res.is_ok(), "device enumeration failed: {res:?}");
    }
}
