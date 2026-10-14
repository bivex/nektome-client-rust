use crate::audio::{AudioSink, resample_pcm_into};
use chrono::Local;
use flacenc::{
    bitsink::ByteSink,
    component::{BitRepr, Stream, StreamInfo},
    config,
    error::{Verified, Verify},
    source::{Context, Fill, FrameBuf},
};
use rusty_mp3::{Mp3Encoder, Mp3EncoderConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Instant, SystemTime},
};

#[cfg(feature = "opus-media")]
use std::io::BufWriter;

const RECORDING_SAMPLE_RATE: u32 = 48_000;
const MP3_FRAME_SAMPLES: usize = 1_152;
// Hold encoded output behind capture so brief callback and network scheduling
// delays can arrive without being mistaken for missing audio.
const ALIGNMENT_DELAY_SAMPLES: u64 = 24_000;
const TIMELINE_REANCHOR_SAMPLES: u64 = ALIGNMENT_DELAY_SAMPLES;
const GAP_FADE_SAMPLES: usize = 240;
const UNSET_POSITION: u64 = u64::MAX;
const RECORDER_BUFFER_COUNT: usize = 32;
const RECORDER_BUFFER_CAPACITY: usize = 5_760;
const RECORDING_GAIN_Q15: i32 = 23_198;
#[cfg(feature = "opus-media")]
const OPUS_FRAME_SAMPLES: usize = 960;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingFormat {
    #[default]
    Flac,
    Mp3,
    Opus,
}

#[derive(Debug, Clone)]
/// Metadata for a finalized `NektoMe` session recording in Downloads.
pub struct SavedRecording {
    pub path: PathBuf,
    pub format: RecordingFormat,
    pub bytes: u64,
    pub modified: SystemTime,
}

/// Lists finalized `NektoMe` FLAC, MP3, and Opus recordings, newest first.
///
/// # Errors
///
/// Returns an error when the Downloads directory cannot be located or read.
pub fn saved_recordings() -> Result<Vec<SavedRecording>, String> {
    let directory = downloads_directory()?;
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut recordings = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let format = recording_format_from_path(&entry.path())?;
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| SavedRecording {
                path: entry.path(),
                format,
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect::<Vec<_>>();
    recordings.sort_unstable_by_key(|recording| std::cmp::Reverse(recording.modified));
    Ok(recordings)
}

/// Deletes a finalized recording managed by this application.
///
/// # Errors
///
/// Returns an error for unmanaged paths or when the file cannot be removed.
pub fn delete_saved_recording(path: &Path) -> Result<(), String> {
    if recording_format_from_path(path).is_none()
        || path.parent() != Some(downloads_directory()?.as_path())
    {
        return Err("refusing to delete a file outside the NektoMe recording library".into());
    }
    std::fs::remove_file(path).map_err(|error| error.to_string())
}

fn recording_format_from_path(path: &Path) -> Option<RecordingFormat> {
    let name = path.file_name()?.to_str()?;
    if !name.starts_with("NektoMe-session-")
        || path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("part"))
    {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("flac") {
        Some(RecordingFormat::Flac)
    } else if extension.eq_ignore_ascii_case("mp3") {
        Some(RecordingFormat::Mp3)
    } else if extension.eq_ignore_ascii_case("opus") {
        Some(RecordingFormat::Opus)
    } else {
        None
    }
}

impl RecordingFormat {
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Mp3 => "MP3",
            Self::Opus => "Opus",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Bit allocation used by the MP3 encoder.
pub enum Mp3RateMode {
    #[default]
    Constant,
    Variable,
}

impl Mp3RateMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Constant => "CBR",
            Self::Variable => "VBR",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Bit allocation used by the Opus encoder.
pub enum OpusRateMode {
    Variable,
    #[default]
    ConstrainedVariable,
    Constant,
}

impl OpusRateMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Variable => "VBR",
            Self::ConstrainedVariable => "Constrained VBR",
            Self::Constant => "CBR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
/// User-configurable encoder settings for saved stereo sessions.
pub struct RecordingParameters {
    /// Whether MP3 recordings use a fixed bitrate or quality target.
    pub mp3_rate_mode: Mp3RateMode,
    /// MP3 constant bitrate in kilobits per second.
    pub mp3_bitrate_kbps: u32,
    /// MP3 VBR quality from 0 (best) through 9 (smallest file).
    pub mp3_vbr_quality: f32,
    /// Opus target bitrate in kilobits per second.
    pub opus_bitrate_kbps: i32,
    /// Opus encoder complexity from 0 through 10.
    pub opus_complexity: i32,
    /// Whether Opus uses variable, constrained-variable, or constant bitrate.
    pub opus_rate_mode: OpusRateMode,
}

impl Default for RecordingParameters {
    fn default() -> Self {
        Self {
            mp3_rate_mode: Mp3RateMode::Constant,
            mp3_bitrate_kbps: 192,
            mp3_vbr_quality: 4.0,
            opus_bitrate_kbps: 128,
            opus_complexity: 10,
            opus_rate_mode: OpusRateMode::ConstrainedVariable,
        }
    }
}

impl RecordingParameters {
    /// Replaces non-finite values and clamps settings to encoder-safe ranges.
    pub fn normalize(&mut self) {
        self.mp3_bitrate_kbps = self.mp3_bitrate_kbps.clamp(32, 320);
        if !self.mp3_vbr_quality.is_finite() {
            self.mp3_vbr_quality = 4.0;
        }
        self.mp3_vbr_quality = self.mp3_vbr_quality.clamp(0.0, 9.0);
        self.opus_bitrate_kbps = self.opus_bitrate_kbps.clamp(32, 256);
        self.opus_complexity = self.opus_complexity.clamp(0, 10);
    }
}

#[derive(Clone, Copy)]
enum Channel {
    Local,
    Remote,
}

enum RecorderMessage {
    Audio {
        channel: Channel,
        pcm: Vec<i16>,
        sample_rate: u32,
        start_position: u64,
    },
    Finish,
}

struct RecorderInner {
    sender: mpsc::SyncSender<RecorderMessage>,
    recycle_sender: mpsc::SyncSender<Vec<i16>>,
    recycled: Mutex<mpsc::Receiver<Vec<i16>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    finishing: AtomicBool,
    path: PathBuf,
    local_position: AtomicU64,
    remote_position: AtomicU64,
}

impl RecorderInner {
    fn finish(&self) {
        if !self.finishing.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(RecorderMessage::Finish);
        }
        let thread = self.thread.lock().ok().and_then(|mut thread| thread.take());
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for RecorderInner {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Records one call as stereo audio: processed local microphone on the left
/// and decoded peer audio on the right. Encoding runs off the real-time audio
/// callbacks. Per-channel sample timelines absorb callback jitter while their
/// wall-clock anchors keep independently arriving streams aligned.
#[derive(Clone)]
pub struct StereoSessionRecorder {
    inner: Arc<RecorderInner>,
    started: Instant,
}

impl StereoSessionRecorder {
    /// Creates a recorder whose finalized file is placed in the user's
    /// Downloads directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the Downloads directory cannot be located or
    /// created, or when the encoder thread cannot be started.
    pub fn create_in_downloads(format: RecordingFormat) -> Result<Self, String> {
        Self::create_in_downloads_with_parameters(format, RecordingParameters::default())
    }

    /// Creates a recorder in Downloads with explicit encoder parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the Downloads directory cannot be located or
    /// created, or when the encoder thread cannot be started.
    pub fn create_in_downloads_with_parameters(
        format: RecordingFormat,
        parameters: RecordingParameters,
    ) -> Result<Self, String> {
        let directory = downloads_directory()?;
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let stamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let extension = format.extension();
        let mut path = directory.join(format!("NektoMe-session-{stamp}.{extension}"));
        if path.exists() || part_path(&path).exists() {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            path = directory.join(format!(
                "NektoMe-session-{stamp}-{}.{extension}",
                &suffix[..8]
            ));
        }
        Self::create_with_parameters(path, format, parameters)
    }

    /// Creates a recorder that finalizes to `path` when its last handle is
    /// dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the output or partial path already exists, the
    /// partial file cannot be created, or the encoder thread cannot be started.
    /// File-system errors encountered by that thread prevent the partial file
    /// from being promoted to the requested output path.
    pub fn create(path: PathBuf) -> Result<Self, String> {
        Self::create_with_format(path, RecordingFormat::Flac)
    }

    /// Creates a recorder using the selected encoded file format.
    ///
    /// # Errors
    ///
    /// Returns an error when the output cannot be created, the selected format
    /// is unavailable in this build, or the encoder thread cannot be started.
    pub fn create_with_format(path: PathBuf, format: RecordingFormat) -> Result<Self, String> {
        Self::create_with_parameters(path, format, RecordingParameters::default())
    }

    /// Creates a recorder using the selected format and encoder parameters.
    ///
    /// # Errors
    ///
    /// Returns an error when the output cannot be created, the selected format
    /// is unavailable in this build, or the encoder thread cannot be started.
    pub fn create_with_parameters(
        path: PathBuf,
        format: RecordingFormat,
        mut parameters: RecordingParameters,
    ) -> Result<Self, String> {
        parameters.normalize();
        #[cfg(not(feature = "opus-media"))]
        if format == RecordingFormat::Opus {
            return Err("Opus recording requires the opus-media feature".into());
        }
        if path.exists() {
            return Err(format!("recording already exists: {}", path.display()));
        }
        let part_path = part_path(&path);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
            .map_err(|error| format!("could not create {}: {error}", part_path.display()))?;
        let (sender, receiver) = mpsc::sync_channel(128);
        let (recycle_sender, recycled) = mpsc::sync_channel(RECORDER_BUFFER_COUNT);
        for _ in 0..RECORDER_BUFFER_COUNT {
            let _ = recycle_sender.try_send(Vec::with_capacity(RECORDER_BUFFER_CAPACITY));
        }
        let output_path = path.clone();
        let thread_part_path = part_path.clone();
        let thread_recycle_sender = recycle_sender.clone();
        let started = Instant::now();
        let thread = match std::thread::Builder::new()
            .name("nektome-session-recorder".into())
            .spawn(move || {
                let complete = run_recorder(
                    file,
                    &receiver,
                    &thread_recycle_sender,
                    format,
                    parameters,
                    started,
                )
                .is_ok()
                    && std::fs::metadata(&thread_part_path)
                        .is_ok_and(|metadata| metadata.len() > 0);
                if !complete || std::fs::rename(&thread_part_path, &output_path).is_err() {
                    let _ = std::fs::remove_file(&thread_part_path);
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = std::fs::remove_file(&part_path);
                return Err(format!("could not start session recorder: {error}"));
            }
        };
        Ok(Self {
            inner: Arc::new(RecorderInner {
                sender,
                recycle_sender,
                recycled: Mutex::new(recycled),
                thread: Mutex::new(Some(thread)),
                finishing: AtomicBool::new(false),
                path,
                local_position: AtomicU64::new(UNSET_POSITION),
                remote_position: AtomicU64::new(UNSET_POSITION),
            }),
            started,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn record_local(&self, pcm: &[i16], sample_rate: u32) {
        self.record(Channel::Local, pcm, sample_rate);
    }

    #[must_use]
    pub fn remote_sink(&self) -> Arc<dyn AudioSink> {
        Arc::new(RemoteRecorderSink(self.clone()))
    }

    /// Finishes the encoder and promotes the temporary `.part` file to the
    /// recording's final path. Safe to call more than once or while stale
    /// audio sink clones still exist.
    pub fn finish(&self) {
        self.inner.finish();
    }

    fn record(&self, channel: Channel, pcm: &[i16], sample_rate: u32) {
        if pcm.is_empty() || sample_rate == 0 || self.inner.finishing.load(Ordering::Acquire) {
            return;
        }
        let sample_count = resampled_sample_count(pcm.len(), sample_rate);
        if sample_count == 0 {
            return;
        }
        let wall_end = duration_samples(self.started.elapsed());
        let timeline = match channel {
            Channel::Local => &self.inner.local_position,
            Channel::Remote => &self.inner.remote_position,
        };
        let start_position = reserve_timeline(timeline, wall_end, sample_count);
        let Some(mut buffer) = self
            .inner
            .recycled
            .lock()
            .ok()
            .and_then(|receiver| receiver.try_recv().ok())
        else {
            return;
        };
        buffer.extend_from_slice(pcm);
        let message = RecorderMessage::Audio {
            channel,
            pcm: buffer,
            sample_rate,
            start_position,
        };
        if let Err(error) = self.inner.sender.try_send(message) {
            let message = match error {
                mpsc::TrySendError::Full(message) | mpsc::TrySendError::Disconnected(message) => {
                    message
                }
            };
            let RecorderMessage::Audio { mut pcm, .. } = message else {
                return;
            };
            pcm.clear();
            let _ = self.inner.recycle_sender.try_send(pcm);
        }
    }
}

struct RemoteRecorderSink(StereoSessionRecorder);

impl AudioSink for RemoteRecorderSink {
    fn write(&self, pcm: &[i16], sample_rate: u32) {
        self.0.record(Channel::Remote, pcm, sample_rate);
    }
}

pub(crate) fn downloads_directory() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .map(|home| home.join("Downloads"))
        .ok_or_else(|| "could not locate the Downloads folder".into())
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn duration_samples(duration: std::time::Duration) -> u64 {
    duration
        .as_secs()
        .saturating_mul(u64::from(RECORDING_SAMPLE_RATE))
        .saturating_add(
            u64::from(duration.subsec_nanos()) * u64::from(RECORDING_SAMPLE_RATE) / 1_000_000_000,
        )
}

fn resampled_sample_count(input_samples: usize, input_rate: u32) -> u64 {
    u64::try_from(input_samples)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(RECORDING_SAMPLE_RATE))
        / u64::from(input_rate)
}

fn reserve_timeline(timeline: &AtomicU64, wall_end: u64, sample_count: u64) -> u64 {
    let wall_start = wall_end.saturating_sub(sample_count);
    let mut current = timeline.load(Ordering::Relaxed);
    loop {
        let start = if current == UNSET_POSITION
            || wall_start > current.saturating_add(TIMELINE_REANCHOR_SAMPLES)
        {
            wall_start
        } else {
            current
        };
        let end = start.saturating_add(sample_count);
        match timeline.compare_exchange_weak(current, end, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return start,
            Err(updated) => current = updated,
        }
    }
}

fn place_samples(queue: &mut VecDeque<i16>, written: u64, start: u64, samples: &[i16]) {
    let queued_end = written.saturating_add(u64::try_from(queue.len()).unwrap_or(u64::MAX));
    let mut gap_inserted = false;
    if start > queued_end {
        let Ok(gap) = usize::try_from(start - queued_end) else {
            return;
        };
        let Some(new_len) = queue.len().checked_add(gap) else {
            return;
        };
        let fade_samples = queue.len().min(GAP_FADE_SAMPLES);
        for (index, sample) in queue.iter_mut().rev().take(fade_samples).enumerate() {
            let numerator = i32::try_from(index).expect("the fade index fits in i32");
            let denominator = i32::try_from(fade_samples).expect("the fade length fits in i32");
            *sample = i16::try_from(i32::from(*sample) * numerator / denominator)
                .expect("fading down keeps an i16 sample in range");
        }
        queue.resize(new_len, 0);
        gap_inserted = true;
    }
    let overlap = queued_end
        .saturating_sub(start)
        .min(u64::try_from(samples.len()).unwrap_or(u64::MAX));
    let skip = usize::try_from(overlap).unwrap_or(samples.len());
    let remaining = &samples[skip..];
    if gap_inserted {
        let fade_samples = remaining.len().min(GAP_FADE_SAMPLES);
        queue.extend(remaining.iter().enumerate().map(|(index, sample)| {
            if index >= fade_samples {
                return *sample;
            }
            let numerator = i32::try_from(index).expect("the fade index fits in i32");
            let denominator = i32::try_from(fade_samples).expect("the fade length fits in i32");
            i16::try_from(i32::from(*sample) * numerator / denominator)
                .expect("fading up keeps an i16 sample in range")
        }));
    } else {
        queue.extend(remaining.iter().copied());
    }
}

fn recording_sample(sample: i16) -> i16 {
    i16::try_from((i32::from(sample) * RECORDING_GAIN_Q15) / 32_768)
        .expect("-3 dB gain keeps an i16 sample in range")
}

fn interleave_frame(
    left: &mut VecDeque<i16>,
    right: &mut VecDeque<i16>,
    stereo: &mut Vec<i16>,
    frame_samples: usize,
) {
    stereo.clear();
    stereo.reserve(frame_samples * 2);
    for _ in 0..frame_samples {
        stereo.push(recording_sample(left.pop_front().unwrap_or(0)));
        stereo.push(recording_sample(right.pop_front().unwrap_or(0)));
    }
}

enum RecordingEncoder {
    Flac {
        config: Box<Verified<config::Encoder>>,
        stream_info: StreamInfo,
        context: Context,
        frame_buffer: FrameBuf,
        pcm: Vec<i32>,
        frame_number: usize,
        file: File,
    },
    Mp3 {
        encoder: Box<Mp3Encoder>,
        file: File,
    },
    #[cfg(feature = "opus-media")]
    Opus {
        encoder: Box<opus_pure::OpusEncoder>,
        writer: opus_pure::OggOpusWriter<BufWriter<File>>,
        packet: Vec<u8>,
        pre_skip: u16,
        samples_written: u64,
    },
}

impl RecordingEncoder {
    fn new(
        format: RecordingFormat,
        parameters: RecordingParameters,
        file: File,
    ) -> Result<Self, String> {
        match format {
            RecordingFormat::Flac => {
                let config = config::Encoder::default()
                    .into_verified()
                    .map_err(|(_, error)| error.to_string())?;
                let frame_samples = config.block_size;
                let stream_info = StreamInfo::new(
                    usize::try_from(RECORDING_SAMPLE_RATE)
                        .expect("the recording sample rate fits in usize"),
                    2,
                    16,
                )
                .map_err(|error| error.to_string())?;
                let mut file = file;
                write_flac_header(&mut file, &stream_info)?;
                Ok(Self::Flac {
                    config: Box::new(config),
                    stream_info,
                    context: Context::new(16, 2),
                    frame_buffer: FrameBuf::with_size(2, frame_samples)
                        .map_err(|error| error.to_string())?,
                    pcm: Vec::with_capacity(frame_samples * 2),
                    frame_number: 0,
                    file,
                })
            }
            RecordingFormat::Mp3 => Ok(Self::Mp3 {
                encoder: Box::new(Mp3Encoder::new(Mp3EncoderConfig {
                    bitrate_kbps: parameters.mp3_bitrate_kbps,
                    vbr_quality: (parameters.mp3_rate_mode == Mp3RateMode::Variable)
                        .then(|| rusty_mp3::vbr_quality_index(parameters.mp3_vbr_quality)),
                })),
                file,
            }),
            #[cfg(feature = "opus-media")]
            RecordingFormat::Opus => {
                let mut encoder = opus_pure::OpusEncoder::new(
                    i32::try_from(RECORDING_SAMPLE_RATE)
                        .expect("the recording sample rate fits in i32"),
                    2,
                    opus_pure::Application::Audio,
                )
                .map_err(|error| error.to_string())?;
                encoder.bitrate_bps = parameters.opus_bitrate_kbps.saturating_mul(1_000);
                encoder.complexity = parameters.opus_complexity;
                encoder.rate_control = match parameters.opus_rate_mode {
                    OpusRateMode::Variable => opus_pure::RateControl::Vbr,
                    OpusRateMode::ConstrainedVariable => opus_pure::RateControl::ConstrainedVbr,
                    OpusRateMode::Constant => opus_pure::RateControl::Cbr,
                };
                let head = opus_pure::OpusHead::for_encoder(&encoder, RECORDING_SAMPLE_RATE);
                let pre_skip = head.pre_skip;
                let writer = opus_pure::OggOpusWriter::new(BufWriter::new(file), head)
                    .map_err(|error| error.to_string())?;
                Ok(Self::Opus {
                    encoder: Box::new(encoder),
                    writer,
                    packet: vec![0; opus_pure::MAX_PACKET_BYTES],
                    pre_skip,
                    samples_written: 0,
                })
            }
            #[cfg(not(feature = "opus-media"))]
            RecordingFormat::Opus => Err("Opus recording requires the opus-media feature".into()),
        }
    }

    fn frame_samples(&self) -> usize {
        match self {
            Self::Flac { config, .. } => config.block_size,
            Self::Mp3 { .. } => MP3_FRAME_SAMPLES,
            #[cfg(feature = "opus-media")]
            Self::Opus { .. } => OPUS_FRAME_SAMPLES,
        }
    }

    fn encode_full_frame(&mut self, stereo: &[i16]) -> Result<(), String> {
        match self {
            Self::Flac {
                config,
                stream_info,
                context,
                frame_buffer,
                pcm,
                frame_number,
                file,
            } => encode_flac_frame(
                config,
                stream_info,
                context,
                frame_buffer,
                pcm,
                frame_number,
                file,
                stereo,
            )?,
            Self::Mp3 { encoder, file } => {
                encoder
                    .push_pcm_s16(stereo, 2, RECORDING_SAMPLE_RATE)
                    .map_err(|error| error.to_string())?;
                while let Ok(packet) = encoder.next_packet() {
                    file.write_all(&packet).map_err(|error| error.to_string())?;
                }
            }
            #[cfg(feature = "opus-media")]
            Self::Opus {
                encoder,
                writer,
                packet,
                samples_written,
                ..
            } => {
                let packet_len = encoder
                    .encode_s16(stereo, OPUS_FRAME_SAMPLES, packet)
                    .map_err(|error| error.to_string())?;
                writer
                    .write_packet(&packet[..packet_len])
                    .map_err(|error| error.to_string())?;
                *samples_written = samples_written.saturating_add(
                    u64::try_from(OPUS_FRAME_SAMPLES).expect("the Opus frame size fits in u64"),
                );
            }
        }
        Ok(())
    }

    fn finish(self, stereo: &mut [i16], tail_samples: usize) -> Result<(), String> {
        match self {
            Self::Flac {
                config,
                mut stream_info,
                mut context,
                mut frame_buffer,
                mut pcm,
                mut frame_number,
                mut file,
            } => {
                if tail_samples > 0 {
                    encode_flac_frame(
                        &config,
                        &mut stream_info,
                        &mut context,
                        &mut frame_buffer,
                        &mut pcm,
                        &mut frame_number,
                        &mut file,
                        &stereo[..tail_samples * 2],
                    )?;
                }
                stream_info.set_md5_digest(&context.md5_digest());
                write_flac_header(&mut file, &stream_info)?;
                file.flush().map_err(|error| error.to_string())?;
            }
            Self::Mp3 {
                mut encoder,
                mut file,
            } => {
                if tail_samples > 0 {
                    encoder
                        .push_pcm_s16(stereo, 2, RECORDING_SAMPLE_RATE)
                        .map_err(|error| error.to_string())?;
                }
                encoder.finish();
                while let Ok(packet) = encoder.next_packet() {
                    file.write_all(&packet).map_err(|error| error.to_string())?;
                }
                file.flush().map_err(|error| error.to_string())?;
            }
            #[cfg(feature = "opus-media")]
            Self::Opus {
                mut encoder,
                mut writer,
                mut packet,
                pre_skip,
                samples_written,
            } => {
                if samples_written > 0 || tail_samples > 0 {
                    let total_samples = samples_written.saturating_add(
                        u64::try_from(tail_samples).expect("the tail length fits in u64"),
                    );
                    let flush_frames =
                        (tail_samples + usize::from(pre_skip)).div_ceil(OPUS_FRAME_SAMPLES);
                    for index in 0..flush_frames {
                        if index > 0 {
                            stereo.fill(0);
                        }
                        let packet_len = encoder
                            .encode_s16(stereo, OPUS_FRAME_SAMPLES, &mut packet)
                            .map_err(|error| error.to_string())?;
                        if index + 1 == flush_frames {
                            let final_granule = u64::from(pre_skip).saturating_add(total_samples);
                            let duration = final_granule
                                .saturating_sub(u64::try_from(writer.granule()).unwrap_or(0));
                            writer
                                .write_packet_with_duration(
                                    &packet[..packet_len],
                                    u32::try_from(duration).unwrap_or(u32::MAX),
                                )
                                .map_err(|error| error.to_string())?;
                        } else {
                            writer
                                .write_packet(&packet[..packet_len])
                                .map_err(|error| error.to_string())?;
                        }
                    }
                }
                let mut output = writer.finish().map_err(|error| error.to_string())?;
                output.flush().map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_flac_frame(
    config: &Verified<config::Encoder>,
    stream_info: &mut StreamInfo,
    context: &mut Context,
    frame_buffer: &mut FrameBuf,
    pcm: &mut Vec<i32>,
    frame_number: &mut usize,
    file: &mut File,
    stereo: &[i16],
) -> Result<(), String> {
    pcm.clear();
    pcm.extend(stereo.iter().map(|sample| i32::from(*sample)));
    (&mut *frame_buffer, &mut *context)
        .fill_interleaved(pcm)
        .map_err(|error| error.to_string())?;
    let frame = flacenc::encode_fixed_size_frame(config, frame_buffer, *frame_number, stream_info)
        .map_err(|error| error.to_string())?;
    stream_info.update_frame_info(&frame);
    let mut encoded = ByteSink::with_capacity(frame.count_bits());
    frame
        .write(&mut encoded)
        .map_err(|error| error.to_string())?;
    file.write_all(encoded.as_slice())
        .map_err(|error| error.to_string())?;
    *frame_number = frame_number.saturating_add(1);
    Ok(())
}

fn write_flac_header(file: &mut File, stream_info: &StreamInfo) -> Result<(), String> {
    let stream = Stream::with_stream_info(stream_info.clone());
    let mut encoded = ByteSink::with_capacity(stream.count_bits());
    stream
        .write(&mut encoded)
        .map_err(|error| error.to_string())?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    file.write_all(encoded.as_slice())
        .map_err(|error| error.to_string())?;
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn run_recorder(
    file: File,
    receiver: &mpsc::Receiver<RecorderMessage>,
    recycled: &mpsc::SyncSender<Vec<i16>>,
    format: RecordingFormat,
    parameters: RecordingParameters,
    started: Instant,
) -> Result<(), String> {
    let mut encoder = RecordingEncoder::new(format, parameters, file)?;
    let frame_samples = encoder.frame_samples();
    let mut written = 0u64;
    let queue_capacity = usize::try_from(ALIGNMENT_DELAY_SAMPLES)
        .expect("the alignment delay fits in usize")
        .saturating_add(RECORDER_BUFFER_CAPACITY);
    let mut left = VecDeque::with_capacity(queue_capacity);
    let mut right = VecDeque::with_capacity(queue_capacity);
    let mut stereo = Vec::with_capacity(frame_samples * 2);
    let mut resampled = Vec::new();
    let mut finishing = false;

    while !finishing {
        match receiver.recv_timeout(std::time::Duration::from_millis(25)) {
            Ok(RecorderMessage::Audio {
                channel,
                mut pcm,
                sample_rate,
                start_position,
            }) => {
                let samples = if sample_rate == RECORDING_SAMPLE_RATE {
                    pcm.as_slice()
                } else {
                    resample_pcm_into(&pcm, sample_rate, RECORDING_SAMPLE_RATE, &mut resampled);
                    &resampled
                };
                match channel {
                    Channel::Local => place_samples(&mut left, written, start_position, samples),
                    Channel::Remote => place_samples(&mut right, written, start_position, samples),
                }
                pcm.clear();
                let _ = recycled.try_send(pcm);
            }
            Ok(RecorderMessage::Finish) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                finishing = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let elapsed = duration_samples(started.elapsed());
        let ready_until = if finishing {
            written.saturating_add(left.len().max(right.len()) as u64)
        } else {
            elapsed.saturating_sub(ALIGNMENT_DELAY_SAMPLES)
        };
        while written.saturating_add(
            u64::try_from(frame_samples).expect("the encoder frame size fits in u64"),
        ) <= ready_until
        {
            interleave_frame(&mut left, &mut right, &mut stereo, frame_samples);
            encoder.encode_full_frame(&stereo)?;
            written = written.saturating_add(
                u64::try_from(frame_samples).expect("the encoder frame size fits in u64"),
            );
        }
    }

    let tail_samples = left.len().max(right.len()).min(frame_samples);
    if tail_samples > 0 {
        interleave_frame(&mut left, &mut right, &mut stereo, frame_samples);
    } else {
        stereo.clear();
        stereo.resize(frame_samples * 2, 0);
    }
    encoder.finish(&mut stereo, tail_samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_recording_path(label: &str) -> PathBuf {
        temporary_recording_path_with_extension(label, "flac")
    }

    fn temporary_recording_path_with_extension(label: &str, extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nektome-{label}-{}.{extension}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn recording_gain_leaves_three_db_of_encoder_headroom() {
        assert_eq!(recording_sample(i16::MAX), 23_197);
        assert_eq!(recording_sample(i16::MIN), -23_198);
    }

    #[test]
    fn recording_library_accepts_only_finalized_session_audio() {
        assert_eq!(
            recording_format_from_path(Path::new("NektoMe-session-call.FLAC")),
            Some(RecordingFormat::Flac)
        );
        assert_eq!(
            recording_format_from_path(Path::new("NektoMe-session-2026-01-01_12-00-00.mp3")),
            Some(RecordingFormat::Mp3)
        );
        assert_eq!(
            recording_format_from_path(Path::new("NektoMe-session-call.OPUS")),
            Some(RecordingFormat::Opus)
        );
        assert_eq!(
            recording_format_from_path(Path::new("NektoMe-session-call.mp3.part")),
            None
        );
        assert_eq!(recording_format_from_path(Path::new("unrelated.mp3")), None);
    }

    #[test]
    fn recording_format_defaults_to_flac() {
        assert_eq!(RecordingFormat::default(), RecordingFormat::Flac);
        assert_eq!(RecordingFormat::default().extension(), "flac");
    }

    #[test]
    fn recording_parameters_are_clamped_to_encoder_ranges() {
        let mut parameters = RecordingParameters {
            mp3_bitrate_kbps: u32::MAX,
            mp3_vbr_quality: f32::NAN,
            opus_bitrate_kbps: 1,
            opus_complexity: 99,
            ..RecordingParameters::default()
        };

        parameters.normalize();

        assert_eq!(parameters.mp3_bitrate_kbps, 320);
        assert!((parameters.mp3_vbr_quality - 4.0).abs() < f32::EPSILON);
        assert_eq!(parameters.opus_bitrate_kbps, 32);
        assert_eq!(parameters.opus_complexity, 10);
    }

    #[test]
    fn sample_timeline_absorbs_callback_jitter() {
        let timeline = AtomicU64::new(UNSET_POSITION);
        assert_eq!(reserve_timeline(&timeline, 1_000, 480), 520);
        assert_eq!(reserve_timeline(&timeline, 1_510, 480), 1_000);
        assert_eq!(reserve_timeline(&timeline, 1_970, 480), 1_480);
    }

    #[test]
    fn sample_timeline_preserves_a_real_capture_stall() {
        let timeline = AtomicU64::new(UNSET_POSITION);
        assert_eq!(reserve_timeline(&timeline, 1_000, 480), 520);
        assert_eq!(reserve_timeline(&timeline, 30_000, 480), 29_520);
    }

    #[test]
    fn inserted_capture_gaps_are_faded_to_avoid_clicks() {
        let mut queue = VecDeque::from(vec![10_000; 300]);
        place_samples(&mut queue, 0, 400, &[10_000; 300]);

        assert_eq!(queue.len(), 700);
        assert_eq!(queue[299], 0);
        assert!(queue[200] < 10_000);
        assert!(queue.range(300..400).all(|sample| *sample == 0));
        assert_eq!(queue[400], 0);
        assert!(queue[500] < 10_000);
        assert_eq!(queue[640], 10_000);
    }

    #[test]
    fn stereo_session_recorder_creates_an_mp3_file() {
        let path = temporary_recording_path_with_extension("session-test", "mp3");
        let cycle: [i16; 16] = [
            0, 8_192, 16_384, 24_576, 32_767, 24_576, 16_384, 8_192, 0, -8_192, -16_384, -24_576,
            -32_768, -24_576, -16_384, -8_192,
        ];
        let local = cycle.into_iter().cycle().take(2_304).collect::<Vec<_>>();
        let remote_samples = local
            .iter()
            .map(|sample| sample.saturating_neg())
            .collect::<Vec<_>>();
        {
            let parameters = RecordingParameters {
                mp3_bitrate_kbps: 128,
                ..RecordingParameters::default()
            };
            let recorder = StereoSessionRecorder::create_with_parameters(
                path.clone(),
                RecordingFormat::Mp3,
                parameters,
            )
            .unwrap();
            recorder.record_local(&local, RECORDING_SAMPLE_RATE);
            let remote = recorder.remote_sink();
            remote.write(&remote_samples, RECORDING_SAMPLE_RATE);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() > 100);
        assert_eq!(bytes[0], 0xff);
        assert_eq!(bytes[1] & 0xe0, 0xe0);
        let header = rusty_mp3::header::FrameHeader::parse(bytes[..4].try_into().unwrap()).unwrap();
        assert_eq!(header.sample_rate, RECORDING_SAMPLE_RATE);
        assert_eq!(header.bitrate_kbps, 128);
        assert_eq!(header.channel_mode.channels(), 2);

        let mut decoder = rusty_mp3::Mp3Decoder::new();
        decoder.push(&bytes);
        decoder.flush();
        let mut left_peak = 0.0f32;
        let mut right_peak = 0.0f32;
        while let Ok(frame) = decoder.next_frame() {
            assert_eq!(frame.sample_rate, RECORDING_SAMPLE_RATE);
            assert_eq!(frame.channels, 2);
            for sample in frame.samples.chunks_exact(2) {
                left_peak = left_peak.max(sample[0].abs());
                right_peak = right_peak.max(sample[1].abs());
            }
        }
        assert!((0.5..0.9).contains(&left_peak));
        assert!((0.5..0.9).contains(&right_peak));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stereo_session_recorder_creates_a_lossless_flac_file() {
        let path = temporary_recording_path("flac-session-test");
        let local = (0..5_000)
            .map(|index| if index % 24 < 12 { 16_000 } else { -16_000 })
            .collect::<Vec<_>>();
        let remote = local.iter().map(|sample| -*sample).collect::<Vec<_>>();
        {
            let recorder = StereoSessionRecorder::create(path.clone()).unwrap();
            recorder.record_local(&local, RECORDING_SAMPLE_RATE);
            recorder.remote_sink().write(&remote, RECORDING_SAMPLE_RATE);
        }

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"fLaC"));
        let mut reader = claxon::FlacReader::open(&path).unwrap();
        let info = reader.streaminfo();
        assert_eq!(info.sample_rate, RECORDING_SAMPLE_RATE);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.samples, Some(5_000));
        let decoded = reader.samples().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(decoded.len(), local.len() * 2);
        for (index, frame) in decoded.chunks_exact(2).enumerate() {
            assert_eq!(frame[0], i32::from(recording_sample(local[index])));
            assert_eq!(frame[1], i32::from(recording_sample(remote[index])));
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(feature = "opus-media")]
    #[test]
    fn stereo_session_recorder_creates_a_decodable_ogg_opus_file() {
        let path = temporary_recording_path_with_extension("opus-session-test", "opus");
        {
            let parameters = RecordingParameters {
                opus_bitrate_kbps: 96,
                opus_complexity: 4,
                opus_rate_mode: OpusRateMode::Constant,
                ..RecordingParameters::default()
            };
            let recorder = StereoSessionRecorder::create_with_parameters(
                path.clone(),
                RecordingFormat::Opus,
                parameters,
            )
            .unwrap();
            let local = (0..2_400)
                .map(|index| if index % 24 < 12 { 16_000 } else { -16_000 })
                .collect::<Vec<_>>();
            let remote = local.iter().map(|sample| -*sample).collect::<Vec<_>>();
            recorder.record_local(&local, RECORDING_SAMPLE_RATE);
            recorder.remote_sink().write(&remote, RECORDING_SAMPLE_RATE);
        }

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"OggS"));
        let mut reader = opus_pure::OggOpusReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.head().channel_count, 2);
        assert_eq!(reader.head().input_sample_rate, RECORDING_SAMPLE_RATE);
        let expected_final_granule = i64::from(reader.head().pre_skip) + 2_400;
        let mut decoder = opus_pure::OpusDecoder::new(48_000, 2).unwrap();
        let mut pcm_output = vec![0i16; opus_pure::MAX_PACKET_SAMPLES * 2];
        let mut packet_count = 0;
        let mut peak = 0i16;
        let mut final_granule = 0;
        while let Some(packet) = reader.read_packet().unwrap() {
            let samples = decoder
                .decode_s16(&packet.data, opus_pure::MAX_PACKET_SAMPLES, &mut pcm_output)
                .unwrap();
            peak = peak.max(
                pcm_output[..samples * 2]
                    .iter()
                    .map(|sample| sample.saturating_abs())
                    .max()
                    .unwrap_or(0),
            );
            final_granule = packet.page_granule;
            packet_count += 1;
        }
        assert!(packet_count > 0);
        assert!(peak > 1_000);
        assert_eq!(final_granule, expected_final_granule);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recorder_does_not_overwrite_existing_or_partial_files() {
        let path = temporary_recording_path("collision-test");
        std::fs::write(&path, b"existing recording").unwrap();
        assert!(StereoSessionRecorder::create(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing recording");

        std::fs::remove_file(&path).unwrap();
        let partial = part_path(&path);
        std::fs::write(&partial, b"unfinished recording").unwrap();
        assert!(StereoSessionRecorder::create(path.clone()).is_err());
        assert_eq!(std::fs::read(&partial).unwrap(), b"unfinished recording");
        assert!(!path.exists());
        let _ = std::fs::remove_file(partial);
    }

    #[test]
    fn recorder_finalizes_after_bursty_input() {
        let path = temporary_recording_path("burst-test");
        let thread_path = path.clone();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let recorder = StereoSessionRecorder::create(thread_path.clone()).unwrap();
            let remote = recorder.remote_sink();
            for _ in 0..2_000 {
                recorder.record_local(&[1_000; 480], RECORDING_SAMPLE_RATE);
                remote.write(&[-1_000; 960], RECORDING_SAMPLE_RATE);
            }
            drop(remote);
            drop(recorder);
            let _ = finished_tx.send(thread_path);
        });

        let finalized = finished_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("recorder teardown timed out");
        assert!(finalized.exists());
        assert!(!part_path(&finalized).exists());
        assert!(std::fs::metadata(&finalized).unwrap().len() > 100);
        let _ = std::fs::remove_file(finalized);
    }

    #[test]
    fn explicit_finish_finalizes_while_audio_sink_clone_is_alive() {
        let path = temporary_recording_path("explicit-finish-test");
        let recorder = StereoSessionRecorder::create(path.clone()).unwrap();
        let remote = recorder.remote_sink();
        recorder.record_local(&[1_000; 960], RECORDING_SAMPLE_RATE);
        remote.write(&[-1_000; 960], RECORDING_SAMPLE_RATE);

        recorder.finish();

        assert!(path.exists());
        assert!(!part_path(&path).exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        remote.write(&[-1_000; 960], RECORDING_SAMPLE_RATE);
        recorder.finish();
        drop(remote);
        drop(recorder);
        let _ = std::fs::remove_file(path);
    }
}
