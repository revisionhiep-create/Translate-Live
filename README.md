# Translate Live

A fast, local, private Windows live stream & VOD audio translator built in Rust (`eframe`/`egui` + statically linked `sherpa-onnx` non-Whisper ASR + Silero VAD + embedded `llama.cpp` with Vulkan GPU acceleration and automatic CPU fallback).

Capture live Windows system audio (WASAPI loopback + optional mic), transcribe Japanese/Korean/Chinese/multilingual speech without Whisper outro hallucinations, and translate in real time with rolling conversational context onto a movable, resizable, transparent subtitle overlay.

---

## Download & Quick Start (Windows 10 / 11 x64)

### 1. Download `Translate-Live.exe`
- **Direct Download:** [**Download `Translate-Live.exe` (~71 MB)**](https://github.com/revisionhiep-create/Translate-Live/raw/main/Translate-Live.exe)
  - Or click **`Translate-Live.exe`** in the repository file list above and choose **Download raw file**.
- Place `Translate-Live.exe` inside its own folder (for example `C:\Translate-Live\Translate-Live.exe`). No installer, Python, Conda, Ollama, or external DLL runtime is needed.
- **Windows SmartScreen note:** Because the open-source binary is unsigned, Windows Defender SmartScreen may show a prompt on first launch — click **More info** → **Run anyway**.

### 2. Download Models (First Run)
No AI models are bundled on startup:
1. Launch `Translate-Live.exe`.
2. Under **Shared Model Storage Folder**, keep `<exe_dir>\models` (created automatically on first download) or click **Browse...** to choose another drive/folder.
3. Choose a **Voice Capture (ASR)** preset matching the stream language (default: **Parakeet TDT-CTC 0.6B (JA)**, or **Paraformer / FireRedASR2** for Chinese, **Zipformer Korean**, **SenseVoice Small** multilingual, etc.) and click **Download Model**.
4. Under **Translation (LLM)**, keep the default **Qwen3.5-4B Uncensored Q4_K_M** (or choose the lighter **Qwen2.5-3B**) and click **Download Model**.

### 3. Translate Any Live Stream or VOD
1. Check **Audio & VAD**: leave **Loopback Device** on your default playback output (`Windows Default Output`) to capture any browser, VOD, media player, or game audio, and pick the **Source Language** (`ja`, `ko`, `zh`, `yue`, `en`, or `auto`) and **Target Language** (`en` default).
2. Drag the transparent **Subtitle Overlay** into position and drag any edge/corner to resize it (window positions save per-monitor automatically).
3. Toggle **Live Capture** (or press **`F8`**) — translations will stream live at the bottom and smoothly scroll up (`3` recent lines visible by default).
4. Press **`F10`** to toggle **Click-Through** mode so clicks pass straight through the subtitle window to your video or game; press **`F9`** anytime to show/hide the overlay.

### System Requirements
- **OS:** Windows 10 or Windows 11 (64-bit)
- **GPU (Recommended):** DirectX/Vulkan-capable GPU with ~4 GB VRAM for the default 4B Q4_K_M translator (falls back automatically to CPU if Vulkan is unavailable).

---

## Key Features

- **Single Self-Contained Windows Executable (`Translate-Live.exe`)**:
  - No Python, Conda, Ollama, or external DLL runtime installation required.
  - No models are bundled or downloaded on startup; models download only when you click **Download Model** (saved by default into `<exe_dir>\models` and configurable via **Browse...**).
- **Zero-Hallucination Local ASR Presets (`sherpa-onnx`)**:
  1. `Parakeet TDT-CTC 0.6B (JA)` — `csukuangfj/sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8` *(Default)*
  2. `Haas JA v1.0 (Quantized)` — `oboroge0/Haas-ja-v1.0-onnx-quantized`
  3. `ReazonSpeech k2 v2 (Zipformer JA)` — `reazon-research/reazonspeech-k2-v2`
  4. `Paraformer Large (ZH / EN)` — `csukuangfj/sherpa-onnx-paraformer-zh-2024-03-09`
  5. `FireRedASR2 CTC (ZH / EN)` — `csukuangfj2/sherpa-onnx-fire-red-asr2-ctc-zh_en-int8-2026-02-25`
  6. `Zipformer Korean (KO)` — `k2-fsa/sherpa-onnx-zipformer-korean-2024-06-24`
  7. `Parakeet TDT 0.6B v2 (EN)` — `csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`
  8. `Parakeet TDT 0.6B v3 (EN + 25 EU)` — `csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`
  9. `SenseVoice Small (JA / KO / ZH / YUE / EN)` — `csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17`
  - Plus a **Custom Hugging Face Link...** option in both ASR and LLM selectors supporting custom HF repos, `/resolve/`, `/blob/`, and `.tar.bz2` archive URLs.
- **VTuber & Noisy Gaming Stream VAD + 3-Line Smooth Scroll-Up Subtitles**:
  - Pre-tuned out of the box for foreground streamer speech over game BGM/SFX (`Silero 62%` threshold, `2.5%` foreground Energy Threshold gate active in both VAD modes, `Adaptive` silence `0.70 s`, `0.30 s–6.50 s` speech window, `0.65 s` live fill cadence).
- **Local Uncensored GPU LLM Translation (`llama.cpp` Vulkan)**:
  - Default: `Qwen3.5-4B-Uncensored-HauhauCS-Aggressive-Q4_K_M.gguf`
  - Uses `LocalAgreement` forced-prefix locking (solid confirmed words + softer dimmed live tail) in **Smooth Live Fill** mode, or zero-flicker **Clean Sentences Only** mode, with **3-Line Smooth Scroll-Up** crawl (older translations glide smoothly upward and gently recede with zero flash on finalize), `1.20 s` rapid-burst min read time, and DRY/repetition penalties.
- **Native Transparent Direct2D/DirectWrite Subtitle Overlay**:
  - Movable and edge-resizable over streams, games, and video players.
  - Real-time opacity (`0%–100%`), background color, corner radius, optional background image, click-through mode, top-most toggle, auto-hide fade timer, and customizable multi-line typography/outline/alignment.
- **Multi-Monitor Persistence & Off-Screen Safeguards**:
  - Independently saves `x`, `y`, `width`, `height`, and `maximized` state for both the Control Panel and Subtitle Overlay in `%APPDATA%\Translate-Live\settings.json` (400 ms debounced + exit save).
  - Supports negative virtual desktop coordinates and automatically recovers windows onto an active monitor (or bottom-center primary for the Subtitle Overlay) if a saved monitor is disconnected or `< 80x40 px` is visible.

---

## Default Global Hotkeys

- `F8` — Start / Stop Live Capture
- `F9` — Show / Hide Subtitle Overlay
- `F10` — Toggle Click-Through Mode

All hotkeys can be rebound, reset, or cleared live inside **Control & Models**.

---

## Building from Source (Windows x64)

### Prerequisites

- **OS**: Windows 10 or Windows 11 (x64)
- **Rust Toolchain**: `stable-x86_64-pc-windows-msvc`
- **Visual Studio 2022 Build Tools**: `Desktop development with C++` (MSVC, Windows SDK, CMake, Ninja)
- **LLVM**: `C:\Program Files\LLVM\bin` (`libclang.dll` for `llama-cpp-2` bindgen)
- **LunarG Vulkan SDK**: e.g. `1.4.363.0` (`glslc.exe`, headers, `vulkan-1.lib`)

### Build & Test

Run the bundled PowerShell build script (which sets up the VS 2022 x64 environment, Ninja, Vulkan SDK, short `CARGO_TARGET_DIR`, and `/MD` static `sherpa-onnx` libraries):

```powershell
.\build.ps1
```

This runs `cargo test`, builds the release binary, and outputs `Translate-Live.exe` in the project root.

### CLI Self-Test

```powershell
.\Translate-Live.exe --self-test --smoke
```

---

## License

This project is licensed under the [MIT License](LICENSE).
