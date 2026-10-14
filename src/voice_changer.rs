//! Real-time AI Voice Changer module using ONNX Runtime (RVC pipeline).
//!
//! Architecture:
//! - ContentVec (HuBERT): extracts phonetic representations from 16kHz audio.
//! - RMVPE: detects fundamental frequency (F0 / pitch) from 16kHz audio.
//! - Mel-scale quantization: maps F0 to 256 coarse bins for RVC v2 embedding table.
//! - Target Synthesizer: converts phonetic features + shifted F0 into target voice.
//! - SOLA (Synchronized Overlap-Add) stitching (see `voice_dsp::Sola`) on a dedicated OS worker thread.
//! - Model output rate is detected from the synthesizer output and converted to 48 kHz.
//! - Playout queue at the microphone device rate with prebuffering (see `voice_dsp::Playout`).

#![allow(clippy::all, clippy::pedantic)]

/// Parameters for the neural voice changer.
#[derive(Debug, Clone)]
pub struct VoiceChangerConfig {
    pub enabled: bool,
    pub voice_model_path: String,
    pub rmvpe_path: String,
    pub content_vec_path: String,
    /// Pitch shift in semitones (0.0 = natural model pitch, +6..+12 = higher female pitch).
    pub pitch_shift: f32,
    /// Dry/Wet mix ratio (0.0 = completely dry, 1.0 = completely changed).
    pub wet: f32,
}

/// Returns the correct ContentVec ONNX path for a given voice model path.
///
/// Models trained on `vec-256-layer-9` (Sabrina, Lana, Donald Trump, etc.) need
/// the 256-dim extractor; all others use the default 768-dim extractor.
pub fn content_vec_path_for_model(voice_model_path: &str) -> &'static str {
    // Extract the file stem (lowercase) to identify the model family.
    let stem = voice_model_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(voice_model_path)
        .trim_end_matches(".onnx")
        .to_ascii_lowercase();

    // Models known to use vec-256-layer-9 (256-dim ContentVec).
    // Verified via onnxruntime: sabrina=256, lana=768, woman_*=768.
    let uses_256 = matches!(stem.as_str(), "sabrina" | "donaldtrump" | "donald_trump");

    if uses_256 {
        "models/base/vec-256-layer-9.onnx"
    } else {
        "models/base/vec-768-layer-12.onnx"
    }
}

impl Default for VoiceChangerConfig {
    fn default() -> Self {
        let voice_model_path = "models/voices/woman_1.onnx".to_owned();
        let content_vec_path = content_vec_path_for_model(&voice_model_path).to_owned();
        Self {
            enabled: false,
            rmvpe_path: "models/base/RMVPE.onnx".into(),
            content_vec_path,
            voice_model_path,
            pitch_shift: 0.0,
            wet: 1.0,
        }
    }
}

#[cfg(feature = "voice-changer")]
mod engine {
    use super::VoiceChangerConfig;
    use ort::session::Session;
    use ort::value::Tensor;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use crate::voice_dsp::{Playout, Sola, model_rate_from_output, to_engine_rate};

    // Audio streaming constants:
    // Hop size: 3200 samples at 16kHz = 200ms = 9600 samples at 48kHz.
    pub(crate) const HOP_SIZE_16K: usize = 3200;
    pub(crate) const HOP_SIZE_48K: usize = 9600;

    // Overlap between consecutive chunks: 320 samples at 16kHz = 20ms = 960 samples at 48kHz.
    pub(crate) const OVERLAP_16K: usize = 320;
    pub(crate) const OVERLAP_48K: usize = 960;

    // SOLA search range: 160 samples at 16kHz = 10ms = 480 samples at 48kHz.
    pub(crate) const SEARCH_16K: usize = 160;
    pub(crate) const SEARCH_48K: usize = 480;

    // Total chunk size fed to model: 3200 + 320 + 160 = 3680 samples at 16kHz = 230ms (23 frames of 160).
    pub(crate) const CHUNK_SIZE_16K: usize = HOP_SIZE_16K + OVERLAP_16K + SEARCH_16K;

    // Left context fed to model for warm-up: 1600 samples at 16kHz = 100ms (10 frames of 160, 5 frames of ContentVec).
    pub(crate) const CONTEXT_16K: usize = 1600;

    // RVC F0 coarse bin constants
    const F0_MIN: f32 = 50.0;
    const F0_MAX: f32 = 1100.0;
    const F0_BIN: f32 = 256.0;

    // Input channel capacity: hold up to 128 x 10ms chunks (1.28s cushion).
    const INPUT_CHANNEL_CAPACITY: usize = 128;

    pub struct RvcEngine {
        rmvpe: Session,
        content_vec: Session,
        voice_model: Session,
        current_model_path: String,
        current_content_vec_path: String,
        f0_mel_min: f32,
        f0_mel_max: f32,
        /// Sample rate of the synthesizer output, detected on the last `convert` call.
        output_rate: u32,
    }

    impl RvcEngine {
        fn build_session(path: &str) -> Result<Session, String> {
            Session::builder()
                .map_err(|e| format!("session builder error: {e}"))?
                .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
                .map_err(|e| format!("session opt error: {e}"))?
                .with_intra_threads(4)
                .map_err(|e| format!("session threads error: {e}"))?
                .commit_from_file(path)
                .map_err(|e| format!("session load error for {path}: {e}"))
        }

        fn detect_phone_dim_from_session(session: &Session) -> Option<usize> {
            for inp in session.inputs() {
                if inp.name() == "phone" {
                    if let ort::value::ValueType::Tensor { shape, .. } = inp.dtype() {
                        if let Some(&dim) = shape.get(2) {
                            if dim > 0 {
                                return Some(dim as usize);
                            }
                        }
                    }
                }
            }
            None
        }

        /// Detects required phone feature dimension (256 or 768) from the voice model's `phone` input.
        pub fn phone_dim(&self) -> Option<usize> {
            Self::detect_phone_dim_from_session(&self.voice_model)
        }

        pub fn new(
            rmvpe_path: &str,
            content_vec_path: &str,
            voice_model_path: &str,
        ) -> Result<Self, String> {
            let voice_model = Self::build_session(voice_model_path)?;
            let detected_dim = Self::detect_phone_dim_from_session(&voice_model);
            let effective_cv_path = match detected_dim {
                Some(256) => "models/base/vec-256-layer-9.onnx",
                Some(768) => "models/base/vec-768-layer-12.onnx",
                _ => content_vec_path,
            };
            let content_vec = Self::build_session(effective_cv_path)?;
            let rmvpe = Self::build_session(rmvpe_path)?;

            let f0_mel_min = 1127.0 * (1.0 + F0_MIN / 700.0).ln();
            let f0_mel_max = 1127.0 * (1.0 + F0_MAX / 700.0).ln();

            Ok(Self {
                rmvpe,
                content_vec,
                voice_model,
                current_model_path: voice_model_path.to_owned(),
                current_content_vec_path: effective_cv_path.to_owned(),
                f0_mel_min,
                f0_mel_max,
                output_rate: 40_000,
            })
        }

        /// Reload ContentVec extractor if the required path differs (model family changed).
        pub fn reload_content_vec_if_needed(
            &mut self,
            content_vec_path: &str,
        ) -> Result<(), String> {
            if self.current_content_vec_path == content_vec_path {
                return Ok(());
            }
            let content_vec = Self::build_session(content_vec_path)?;
            self.content_vec = content_vec;
            self.current_content_vec_path = content_vec_path.to_owned();
            Ok(())
        }

        pub fn reload_voice_model(&mut self, new_path: &str) -> Result<(), String> {
            if self.current_model_path == new_path {
                return Ok(());
            }
            let voice_model = Self::build_session(new_path)?;
            let detected_dim = Self::detect_phone_dim_from_session(&voice_model);
            let effective_cv_path = match detected_dim {
                Some(256) => "models/base/vec-256-layer-9.onnx",
                Some(768) => "models/base/vec-768-layer-12.onnx",
                _ => super::content_vec_path_for_model(new_path),
            };
            self.reload_content_vec_if_needed(effective_cv_path)?;
            self.voice_model = voice_model;
            self.current_model_path = new_path.to_owned();
            Ok(())
        }

        /// Converts 16 kHz audio with optional left context.
        ///
        /// When `context_samples > 0`, ContentVec, RMVPE, and the Synthesizer
        /// process `[context + chunk]` (warming up transformer self-attention,
        /// BiGRU recurrence, and vocoder dilated convolutions), and the context
        /// audio is discarded from the output.
        pub fn convert_with_context(
            &mut self,
            audio_16k: &[f32],
            context_samples: usize,
            pitch_shift_semitones: f32,
        ) -> Result<Vec<f32>, String> {
            let sample_len = audio_16k.len();
            if sample_len < context_samples + 320 {
                return Ok(Vec::new());
            }

            // 1. ContentVec: pad by 320 zeros on right to avoid convolution edge shrinkage.
            let mut cv_input = audio_16k.to_vec();
            cv_input.extend_from_slice(&[0.0f32; 320]);
            let cv_len = cv_input.len();
            let source_tensor = Tensor::from_array(([1usize, 1usize, cv_len], cv_input))
                .map_err(|e| format!("content_vec input: {e}"))?;
            let cv_outputs = self
                .content_vec
                .run(ort::inputs![source_tensor])
                .map_err(|e| format!("content_vec run: {e}"))?;
            let (cv_shape, cv_data) = cv_outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("content_vec extract: {e}"))?;

            let cv_frames = cv_shape[1] as usize;
            let hidden_dim = cv_shape[2] as usize;

            // 2x upsampling (HuBERT 20ms -> RVC 10ms) using nearest repeat (matching official RVC F.interpolate)
            let phone_frames = cv_frames * 2;
            let mut phone_data = vec![0.0f32; phone_frames * hidden_dim];
            for f in 0..cv_frames {
                let src_offset = f * hidden_dim;
                let src_frame = &cv_data[src_offset..src_offset + hidden_dim];
                let dst_offset_1 = (f * 2) * hidden_dim;
                let dst_offset_2 = (f * 2 + 1) * hidden_dim;
                phone_data[dst_offset_1..dst_offset_1 + hidden_dim].copy_from_slice(src_frame);
                phone_data[dst_offset_2..dst_offset_2 + hidden_dim].copy_from_slice(src_frame);
            }

            // 2. RMVPE: input [1, sample_len], threshold []
            let wav_tensor = Tensor::from_array(([1usize, sample_len], audio_16k.to_vec()))
                .map_err(|e| format!("rmvpe input: {e}"))?;
            let thresh_tensor =
                Tensor::from_array(((), vec![0.05f32])).map_err(|e| format!("thresh: {e}"))?;
            let rmvpe_outputs = self
                .rmvpe
                .run(ort::inputs![wav_tensor, thresh_tensor])
                .map_err(|e| format!("rmvpe run: {e}"))?;
            let (f0_shape, f0_data) = rmvpe_outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("rmvpe f0 extract: {e}"))?;

            let rmvpe_frames = f0_shape[1] as usize;
            let total_frames = (sample_len / 160).min(phone_frames).min(rmvpe_frames);
            let context_frames = context_samples / 160;
            if total_frames <= context_frames {
                return Ok(Vec::new());
            }

            // Compute pitch factor (1.0 = unmodified natural pitch of speaker)
            let pitch_factor = if pitch_shift_semitones.abs() < 1e-4 {
                1.0
            } else {
                2.0f32.powf(pitch_shift_semitones / 12.0)
            };

            let mut pitchf = vec![0.0f32; total_frames];
            // In RVC v2, unvoiced frames are mapped to coarse bin 1
            let mut pitch = vec![1i64; total_frames];

            let mel_range = self.f0_mel_max - self.f0_mel_min;

            for i in 0..total_frames {
                let orig_f0 = f0_data[i];
                if orig_f0 > F0_MIN {
                    let shifted = orig_f0 * pitch_factor;
                    pitchf[i] = shifted;
                    let f0_mel = 1127.0 * (1.0 + shifted / 700.0).ln();
                    let bin = (f0_mel - self.f0_mel_min) * (F0_BIN - 2.0) / mel_range + 1.0;
                    pitch[i] = bin.round().clamp(1.0, 255.0) as i64;
                } else {
                    pitchf[i] = 0.0;
                    pitch[i] = 1;
                }
            }

            let target_frames = total_frames.saturating_sub(context_frames);
            if target_frames == 0 {
                return Ok(Vec::new());
            }

            // Slice out warmed-up context features: feed only target_frames to the synthesizer.
            let phone_start = context_frames * hidden_dim;
            let phone_end = (context_frames + target_frames) * hidden_dim;
            let phone_slice = &phone_data[phone_start..phone_end];

            let pitch_slice = pitch[context_frames..context_frames + target_frames].to_vec();
            let pitchf_slice = pitchf[context_frames..context_frames + target_frames].to_vec();

            // 3. Synthesizer model
            let phone_tensor =
                Tensor::from_array(([1usize, target_frames, hidden_dim], phone_slice.to_vec()))
                    .map_err(|e| format!("phone tensor: {e}"))?;
            let phone_lengths_tensor = Tensor::from_array(([1usize], vec![target_frames as i64]))
                .map_err(|e| format!("phone_lengths: {e}"))?;
            let pitch_tensor = Tensor::from_array(([1usize, target_frames], pitch_slice))
                .map_err(|e| format!("pitch tensor: {e}"))?;
            let pitchf_tensor = Tensor::from_array(([1usize, target_frames], pitchf_slice))
                .map_err(|e| format!("pitchf tensor: {e}"))?;
            let ds_tensor =
                Tensor::from_array(([1usize], vec![0i64])).map_err(|e| format!("ds: {e}"))?;

            // Standard normal noise N(0, 0.667²) via Box-Muller.
            let mut rng = rand::rng();
            let mut rnd_data = vec![0.0f32; 192 * target_frames];
            let mut it = rnd_data.iter_mut();
            while let (Some(a), Some(b)) = (it.next(), it.next()) {
                use rand::distr::{Distribution as _, StandardUniform};
                let u1: f32 = StandardUniform.sample(&mut rng);
                let u2: f32 = StandardUniform.sample(&mut rng);
                let r = (-2.0 * u1.max(1e-7).ln()).sqrt() * 0.667;
                let theta = 2.0 * std::f32::consts::PI * u2;
                *a = r * theta.cos();
                *b = r * theta.sin();
            }
            let rnd_tensor = Tensor::from_array(([1usize, 192usize, target_frames], rnd_data))
                .map_err(|e| format!("rnd tensor: {e}"))?;

            let synth_outputs = self
                .voice_model
                .run(ort::inputs![
                    phone_tensor,
                    phone_lengths_tensor,
                    pitch_tensor,
                    pitchf_tensor,
                    ds_tensor,
                    rnd_tensor
                ])
                .map_err(|e| format!("synth run: {e}"))?;

            let (_, out_audio) = synth_outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("synth extract: {e}"))?;

            // Detect the model's output rate (32k/40k/48k...) from the output length:
            // the synthesizer emits one hop per 10 ms feature frame.
            self.output_rate =
                model_rate_from_output(out_audio.len(), target_frames).ok_or_else(|| {
                    format!(
                        "unexpected synth output length {} for {target_frames} frames",
                        out_audio.len()
                    )
                })?;

            Ok(out_audio.to_vec())
        }

        pub fn convert(
            &mut self,
            audio_16k: &[f32],
            pitch_shift_semitones: f32,
        ) -> Result<Vec<f32>, String> {
            self.convert_with_context(audio_16k, 0, pitch_shift_semitones)
        }

        /// Like [`Self::convert_with_context`], but returns audio resampled to 48 kHz
        /// regardless of the voice model's native sample rate.
        pub fn convert_48k_with_context(
            &mut self,
            audio_16k: &[f32],
            context_samples: usize,
            pitch_shift_semitones: f32,
        ) -> Result<Vec<f32>, String> {
            let raw =
                self.convert_with_context(audio_16k, context_samples, pitch_shift_semitones)?;
            Ok(to_engine_rate(&raw, self.output_rate))
        }

        /// Like [`Self::convert`], but returns audio resampled to 48 kHz
        /// regardless of the voice model's native sample rate.
        pub fn convert_48k(
            &mut self,
            audio_16k: &[f32],
            pitch_shift_semitones: f32,
        ) -> Result<Vec<f32>, String> {
            self.convert_48k_with_context(audio_16k, 0, pitch_shift_semitones)
        }
    }

    use mkaudiolibrary::dsp::iir::{BiquadType, IirFilter};

    /// Background worker that receives 16kHz audio from mic and streams converted 48kHz audio.
    pub struct VoiceChangerWorker {
        shutdown: Arc<AtomicBool>,
        thread_handle: Option<JoinHandle<()>>,
        input_tx: mpsc::SyncSender<Vec<f32>>,
        output_rx: Arc<Mutex<mpsc::Receiver<Vec<f32>>>>,
        config: Arc<Mutex<VoiceChangerConfig>>,
    }

    impl VoiceChangerWorker {
        pub fn start(config: VoiceChangerConfig) -> Result<Self, String> {
            let (input_tx, input_rx) = mpsc::sync_channel::<Vec<f32>>(INPUT_CHANNEL_CAPACITY);
            // Bounded channel: keep at most 32 chunks (~6.4s) to prevent unbounded memory growth.
            let (output_tx, output_rx) = mpsc::sync_channel::<Vec<f32>>(32);
            let shutdown = Arc::new(AtomicBool::new(false));
            let shared_config = Arc::new(Mutex::new(config.clone()));

            let thread_shutdown = Arc::clone(&shutdown);
            let thread_config = Arc::clone(&shared_config);

            let thread_handle = std::thread::Builder::new()
                .name("voice-changer-worker".into())
                .spawn(move || {
                    let mut engine: Option<RvcEngine> = None;
                    let mut input_buffer: Vec<f32> = vec![0.0f32; CONTEXT_16K];
                    let mut sola = Sola::new(HOP_SIZE_48K, OVERLAP_48K, SEARCH_48K);

                    while !thread_shutdown.load(Ordering::Relaxed) {
                        // Receive audio chunk from microphone
                        match input_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                            Ok(chunk) => {
                                input_buffer.extend(chunk);
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                        // Drain all remaining queued chunks immediately to prevent channel backup and frame drops
                        while let Ok(chunk) = input_rx.try_recv() {
                            input_buffer.extend(chunk);
                        }

                        let (enabled, model_path, pitch_shift) = {
                            let cfg = thread_config.lock().unwrap();
                            (cfg.enabled, cfg.voice_model_path.clone(), cfg.pitch_shift)
                        };

                        if !enabled {
                            input_buffer.clear();
                            input_buffer.resize(CONTEXT_16K, 0.0);
                            sola.reset();
                            continue;
                        }

                        // Initialize or update engine if model changed
                        if engine.is_none() {
                            let cfg = thread_config.lock().unwrap();
                            let cv_path = super::content_vec_path_for_model(&cfg.voice_model_path);
                            eprintln!(
                                "VoiceChangerWorker: init model={} cv={cv_path}",
                                cfg.voice_model_path
                            );
                            match RvcEngine::new(&cfg.rmvpe_path, cv_path, &cfg.voice_model_path) {
                                Ok(eng) => engine = Some(eng),
                                Err(err) => {
                                    eprintln!("VoiceChangerWorker init failed: {err}");
                                    std::thread::sleep(std::time::Duration::from_millis(200));
                                    continue;
                                }
                            }
                        } else if let Some(ref mut eng) = engine {
                            if eng.current_model_path != model_path {
                                match eng.reload_voice_model(&model_path) {
                                    Ok(()) => {
                                        sola.reset();
                                    }
                                    Err(err) => {
                                        eprintln!(
                                            "VoiceChangerWorker: voice model reload failed: {err}"
                                        );
                                        engine = None;
                                    }
                                }
                            }
                        }

                        // Keep latency bounded: if input_buffer accumulated excess chunks (e.g. during initial model load),
                        // discard oldest data so we don't lag behind live audio.
                        while input_buffer.len() > CONTEXT_16K + 2 * CHUNK_SIZE_16K {
                            input_buffer.drain(..HOP_SIZE_16K);
                            sola.reset();
                        }

                        // Process chunks with left context:
                        // input window is CONTEXT_16K (1600 = 100ms) + CHUNK_SIZE_16K (3680 = 230ms).
                        // convert_48k_with_context warms up feature extractors & vocoder on context,
                        // drops the context audio, and returns exactly CHUNK_SIZE_48K (11040) samples.
                        // sola stitches them and emits exactly HOP_SIZE_48K (9600) samples.
                        while input_buffer.len() >= CONTEXT_16K + CHUNK_SIZE_16K {
                            let window = &input_buffer[..CONTEXT_16K + CHUNK_SIZE_16K];
                            if let Some(ref mut eng) = engine {
                                match eng.convert_48k_with_context(window, CONTEXT_16K, pitch_shift)
                                {
                                    Ok(out_48k) => {
                                        if let Some(chunk_out) = sola.push(&out_48k) {
                                            let _ = output_tx.try_send(chunk_out);
                                        }
                                    }
                                    Err(err) => {
                                        eprintln!("RVC conversion error: {err}");
                                        sola.reset();
                                    }
                                }
                            }
                            // Advance buffer by HOP_SIZE_16K (3200 samples = 200ms).
                            // Because CONTEXT_16K <= HOP_SIZE_16K, the head of input_buffer
                            // naturally becomes the continuous left context for the next hop.
                            input_buffer.drain(..HOP_SIZE_16K);
                        }
                    }
                })
                .map_err(|e| format!("Failed to spawn voice-changer thread: {e}"))?;

            Ok(Self {
                shutdown,
                thread_handle: Some(thread_handle),
                input_tx,
                output_rx: Arc::new(Mutex::new(output_rx)),
                config: shared_config,
            })
        }
    }

    impl Drop for VoiceChangerWorker {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.thread_handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Processor embedded in MicrophoneProcessor.
    pub struct VoiceChangerProcessor {
        worker: Option<VoiceChangerWorker>,
        config: VoiceChangerConfig,
        /// Microphone (device rate) -> 16 kHz resampler feeding the worker.
        resampler: crate::audio::LinearResampler,
        resample_i16_buf: Vec<i16>,
        /// Anti-aliasing low-pass filter (4th-order Butterworth at 7.2 kHz) applied before decimation.
        aa_filter: Option<IirFilter>,
        aa_filter_rate: u32,
        scratch_filtered: Vec<i16>,
        /// High-pass filter (2nd-order Butterworth at 48 Hz) applied to 16 kHz audio to remove DC/rumble.
        hp_filter: IirFilter,
        /// Converted audio queued at the microphone device rate.
        playout: Playout,
    }

    impl VoiceChangerProcessor {
        pub fn new() -> Self {
            let config = VoiceChangerConfig::default();
            let worker = VoiceChangerWorker::start(config.clone()).ok();
            Self {
                worker,
                config,
                resampler: crate::audio::LinearResampler::new(),
                resample_i16_buf: Vec::with_capacity(1024),
                aa_filter: None,
                aa_filter_rate: 0,
                scratch_filtered: Vec::with_capacity(1024),
                hp_filter: IirFilter::butterworth(BiquadType::HighPass, 2, 16_000.0, 48.0),
                playout: Playout::new(),
            }
        }

        pub fn set_enabled(&mut self, enabled: bool) {
            self.config.enabled = enabled;
            if enabled {
                self.playout.reset();
                self.resampler.reset();
                self.resample_i16_buf.clear();
                self.scratch_filtered.clear();
                if let Some(ref mut filter) = self.aa_filter {
                    filter.reset();
                }
                self.hp_filter.reset();
            }
            if let Some(ref w) = self.worker {
                if let Ok(mut cfg) = w.config.lock() {
                    cfg.enabled = enabled;
                }
            }
        }

        pub fn is_enabled(&self) -> bool {
            self.config.enabled
        }

        pub fn set_pitch_shift(&mut self, pitch_shift: f32) {
            self.config.pitch_shift = pitch_shift;
            if let Some(ref w) = self.worker {
                if let Ok(mut cfg) = w.config.lock() {
                    cfg.pitch_shift = pitch_shift;
                }
            }
        }

        pub fn set_wet(&mut self, wet: f32) {
            self.config.wet = wet.clamp(0.0, 1.0);
            if let Some(ref w) = self.worker {
                if let Ok(mut cfg) = w.config.lock() {
                    cfg.wet = self.config.wet;
                }
            }
        }

        pub fn set_voice_model_path(&mut self, path: &str) {
            let cv_path = super::content_vec_path_for_model(path).to_owned();
            self.config.voice_model_path = path.to_owned();
            self.config.content_vec_path = cv_path.clone();
            if let Some(ref w) = self.worker {
                if let Ok(mut cfg) = w.config.lock() {
                    cfg.voice_model_path = path.to_owned();
                    cfg.content_vec_path = cv_path;
                }
            }
        }

        /// Process live microphone buffer at given sample rate.
        pub fn process(&mut self, pcm: &mut [i16], sample_rate: u32) {
            if !self.config.enabled || self.worker.is_none() || pcm.is_empty() {
                return;
            }

            let Some(ref worker) = self.worker else {
                return;
            };

            // 1. Anti-aliasing filter and resampling to 16 kHz.
            // If microphone sample rate > 16 kHz, apply a 4th-order Butterworth low-pass filter
            // at 7.2 kHz before decimation. This prevents frequencies above 8 kHz from folding
            // into the 0-8 kHz band as metallic aliasing distortion.
            let input_for_resample: &[i16] = if sample_rate > 16_000 {
                if self.aa_filter_rate != sample_rate || self.aa_filter.is_none() {
                    self.aa_filter = Some(IirFilter::butterworth(
                        BiquadType::LowPass,
                        4,
                        sample_rate as f32,
                        7_200.0,
                    ));
                    self.aa_filter_rate = sample_rate;
                }
                if let Some(ref mut filter) = self.aa_filter {
                    self.scratch_filtered.clear();
                    self.scratch_filtered.reserve(pcm.len());
                    for &s in pcm.iter() {
                        let filtered = filter.process(s as f32);
                        self.scratch_filtered
                            .push(filtered.clamp(-32768.0, 32767.0) as i16);
                    }
                    &self.scratch_filtered
                } else {
                    pcm
                }
            } else {
                pcm
            };

            self.resampler.resample_into(
                input_for_resample,
                sample_rate,
                16_000,
                &mut self.resample_i16_buf,
            );

            // 2. High-pass filter at 48 Hz (removes DC offset and sub-bass microphone rumble)
            // and send normalized f32 audio to the worker.
            if !self.resample_i16_buf.is_empty() {
                let audio_16k: Vec<f32> = self
                    .resample_i16_buf
                    .iter()
                    .map(|&s| {
                        let norm = s as f32 / 32768.0;
                        let hp = self.hp_filter.process(norm);
                        hp.clamp(-1.0, 1.0)
                    })
                    .collect();
                let _ = worker.input_tx.try_send(audio_16k);
            }

            // 3. Drain converted 48kHz audio from the worker into the playout queue,
            // which resamples it to the microphone device rate.
            if let Ok(rx) = worker.output_rx.try_lock() {
                while let Ok(chunk) = rx.try_recv() {
                    self.playout.push_converted(&chunk, sample_rate);
                }
            }

            // 4. Prebuffer, then mix converted audio with dry audio at the device rate.
            self.playout.mix_into(pcm, sample_rate, self.config.wet);
        }
    }

    impl Default for VoiceChangerProcessor {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(feature = "voice-changer")]
pub use engine::{RvcEngine, VoiceChangerProcessor};

#[cfg(not(feature = "voice-changer"))]
#[derive(Debug, Default, Clone)]
pub struct VoiceChangerProcessor;

#[cfg(not(feature = "voice-changer"))]
impl VoiceChangerProcessor {
    pub fn new() -> Self {
        Self
    }
    pub fn set_enabled(&mut self, _enabled: bool) {}
    pub fn is_enabled(&self) -> bool {
        false
    }
    pub fn set_pitch_shift(&mut self, _pitch: f32) {}
    pub fn set_wet(&mut self, _wet: f32) {}
    pub fn set_voice_model_path(&mut self, _path: &str) {}
    pub fn process(&mut self, _pcm: &mut [i16], _sample_rate: u32) {}
}

#[cfg(all(test, feature = "voice-changer"))]
mod tests {
    use super::engine::RvcEngine;

    #[test]
    fn test_rvc_end_to_end_synthesis() {
        let rmvpe_path = "models/base/RMVPE.onnx";
        let vec_path = "models/base/vec-768-layer-12.onnx";
        let woman_path = "models/voices/woman_1.onnx";

        if std::path::Path::new(rmvpe_path).exists()
            && std::path::Path::new(vec_path).exists()
            && std::path::Path::new(woman_path).exists()
        {
            let mut engine =
                RvcEngine::new(rmvpe_path, vec_path, woman_path).expect("RvcEngine::new failed");
            assert_eq!(engine.phone_dim(), Some(768));

            // 0.5s of 16kHz audio (8000 samples) at 220Hz
            let n_samples = 8000usize;
            let mut audio_16k = vec![0.0f32; n_samples];
            for (i, sample) in audio_16k.iter_mut().enumerate() {
                let t = i as f32 / 16000.0;
                *sample = (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.7;
            }

            // Test with 0.0 semitones (natural pitch)
            let converted_natural = engine.convert(&audio_16k, 0.0).expect("convert 0st failed");
            assert!(!converted_natural.is_empty());

            // Test with +12.0 semitones (octave up)
            let converted_shifted = engine
                .convert(&audio_16k, 12.0)
                .expect("convert 12st failed");
            assert!(!converted_shifted.is_empty());
        }
    }

    #[test]
    fn test_content_vec_path_routing() {
        use super::content_vec_path_for_model;
        assert_eq!(
            content_vec_path_for_model("models/voices/sabrina.onnx"),
            "models/base/vec-256-layer-9.onnx"
        );
        assert_eq!(
            content_vec_path_for_model("models/voices/lana.onnx"),
            "models/base/vec-768-layer-12.onnx"
        );
        assert_eq!(
            content_vec_path_for_model("models/voices/woman_1.onnx"),
            "models/base/vec-768-layer-12.onnx"
        );
        assert_eq!(
            content_vec_path_for_model("models/voices/woman_2.onnx"),
            "models/base/vec-768-layer-12.onnx"
        );
    }

    #[test]
    fn test_chunk_duration_exact_ratio() {
        use super::engine::{CHUNK_SIZE_16K, HOP_SIZE_48K, OVERLAP_48K, SEARCH_48K};
        use crate::voice_dsp::to_engine_rate;
        // CHUNK_SIZE_16K is 3680 samples (23 frames of 160).
        // Whatever the model rate (32k/40k/48k), the 48 kHz output must be
        // HOP + OVERLAP + SEARCH = 9600 + 960 + 480 = 11040 samples.
        let expected_48k = HOP_SIZE_48K + OVERLAP_48K + SEARCH_48K;
        let synth_frames = CHUNK_SIZE_16K / 160;
        for rate in [32_000usize, 40_000, 48_000] {
            let raw = vec![0.0f32; synth_frames * rate / 100];
            assert_eq!(to_engine_rate(&raw, rate as u32).len(), expected_48k);
        }
        assert_eq!(expected_48k, 11040);
    }

    #[test]
    fn test_rvc_chunk_latency() {
        let rmvpe_path = "models/base/RMVPE.onnx";
        let vec_path = "models/base/vec-768-layer-12.onnx";
        let woman_path = "models/voices/woman_2.onnx";

        if std::path::Path::new(rmvpe_path).exists()
            && std::path::Path::new(vec_path).exists()
            && std::path::Path::new(woman_path).exists()
        {
            let mut engine =
                RvcEngine::new(rmvpe_path, vec_path, woman_path).expect("RvcEngine::new failed");

            // Warmup
            let window_len = super::engine::CONTEXT_16K + super::engine::CHUNK_SIZE_16K;
            let chunk_ctx = vec![0.05f32; window_len];
            let _ = engine.convert_48k_with_context(&chunk_ctx, super::engine::CONTEXT_16K, 0.0);

            // Measure 3 chunks with 100ms context (5280 samples = 33 frames)
            let mut times = Vec::new();
            for _ in 0..3 {
                let start = std::time::Instant::now();
                let out = engine
                    .convert_48k_with_context(&chunk_ctx, super::engine::CONTEXT_16K, 0.0)
                    .expect("convert failed");
                times.push(start.elapsed().as_millis());
                assert_eq!(
                    out.len(),
                    super::engine::HOP_SIZE_48K
                        + super::engine::OVERLAP_48K
                        + super::engine::SEARCH_48K
                );
            }
            eprintln!("RVC chunk conversion times (100ms context + 200ms chunk): {times:?} ms");
            let budget = if cfg!(debug_assertions) { 350 } else { 200 };
            for &t in &times {
                assert!(
                    t < budget,
                    "Chunk conversion took {t}ms, exceeding {budget}ms budget!"
                );
            }
        }
    }

    #[test]
    fn test_dynamic_phone_dim_detection() {
        let rmvpe_path = "models/base/RMVPE.onnx";
        let vec_768_path = "models/base/vec-768-layer-12.onnx";
        let sabrina_path = "models/voices/sabrina.onnx";

        if std::path::Path::new(rmvpe_path).exists() && std::path::Path::new(sabrina_path).exists()
        {
            // Sabrina was trained on vec-256 (dim 256). Even if we pass vec_768_path,
            // RvcEngine detects dim 256 and automatically routes to vec-256-layer-9.onnx!
            let engine = RvcEngine::new(rmvpe_path, vec_768_path, sabrina_path)
                .expect("RvcEngine::new failed");
            assert_eq!(engine.phone_dim(), Some(256));
        }
    }
}
