#[cfg(feature = "webrtc-media")]
use bytes::Bytes;
#[cfg(feature = "webrtc-media")]
use std::sync::Arc;
#[cfg(feature = "webrtc-media")]
use std::sync::Mutex;
#[cfg(feature = "webrtc-media")]
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
use std::sync::mpsc as sync_mpsc;
#[cfg(feature = "webrtc-media")]
use std::time::Instant;
#[cfg(feature = "webrtc-media")]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(feature = "webrtc-media")]
use tokio::sync::Mutex as AsyncMutex;
#[cfg(feature = "webrtc-media")]
use tokio::sync::mpsc;
#[cfg(feature = "webrtc-media")]
use tokio::sync::watch;

#[cfg(feature = "webrtc-media")]
use rtc::{
    peer_connection::configuration::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_PCMA, MIME_TYPE_PCMU},
    },
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
    },
};

#[cfg(feature = "webrtc-media")]
use webrtc::{
    media_stream::{
        MediaStreamTrack,
        track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP},
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceConnectionState, RTCIceGatheringState,
        RTCIceServer, RTCPeerConnectionIceEvent, RTCSessionDescription, Registry,
    },
};

#[cfg(all(feature = "webrtc-media", feature = "opus-media"))]
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;

#[cfg(feature = "webrtc-media")]
type RemoteStartedCallback = Arc<dyn Fn() + Send + Sync>;

#[cfg(feature = "webrtc-media")]
type RemoteStarted = Arc<Mutex<Option<RemoteStartedCallback>>>;

#[cfg(feature = "webrtc-media")]
fn rtc_ice_server(server: &crate::voice::IceServer) -> RTCIceServer {
    let username = server.username.clone().unwrap_or_default();
    let credential = server.credential.clone().unwrap_or_default();
    RTCIceServer {
        urls: server.urls.clone(),
        username,
        credential,
    }
}

#[cfg(feature = "webrtc-media")]
const PCMU_FRAME_BYTES: usize = 160;
#[cfg(feature = "opus-media")]
const OPUS_FRAME_SAMPLES: usize = 960;
#[cfg(feature = "opus-media")]
#[allow(dead_code)]
const OPUS_MAX_FRAME_SAMPLES: usize = 1_920;
#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
const MICROPHONE_CAPTURE_BUFFERS: usize = 32;
#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
const MICROPHONE_CAPTURE_BUFFER_SAMPLES: usize = 4_096;
#[cfg(feature = "webrtc-media")]
const RTP_RECEIVE_QUEUE: usize = 64;
#[cfg(feature = "webrtc-media")]
const MAX_JITTER_PACKETS: usize = 64;
#[cfg(feature = "webrtc-media")]
/// Consecutive packet-less concealment frames before the stream is treated as
/// stalled and rebuffered.
const MAX_BLIND_CONCEALMENTS: u32 = 3;
#[cfg(feature = "webrtc-media")]
const MIN_PLAYOUT_DELAY_MS: u64 = 60;
#[cfg(feature = "webrtc-media")]
const MAX_PLAYOUT_DELAY_MS: u64 = 200;

#[cfg(feature = "webrtc-media")]
static REMOTE_PACKETS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_LOSS_PERCENT_BITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_JITTER_MS_BITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_DECODE_ERROR_PERCENT_BITS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_LAST_PACKET_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_PLAYOUT_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_PLAYOUT_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_CONCEALED_FRAMES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "webrtc-media")]
static REMOTE_PLAYOUT_FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "webrtc-media")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Current health category for incoming call audio.
pub enum AudioSignalState {
    /// Too few packets have arrived to produce a useful estimate.
    Waiting,
    /// Packet delivery and decoding are stable.
    Good,
    /// Some degradation is measurable but audio should remain usable.
    Fair,
    /// Frequent loss, jitter, or decode errors are likely audible.
    Poor,
    /// Incoming RTP audio has stopped arriving.
    Interrupted,
}

#[cfg(feature = "webrtc-media")]
#[derive(Clone, Copy, Debug)]
/// A lightweight snapshot of recently received RTP audio.
pub struct AudioSignalQuality {
    /// Number of packets observed since the current peer connected.
    pub packets: u64,
    /// Exponentially weighted estimate of missing RTP packets.
    pub loss_percent: f64,
    /// Exponentially weighted packet-arrival jitter in milliseconds.
    pub jitter_ms: f64,
    /// Exponentially weighted percentage of packets that failed to decode.
    pub decode_error_percent: f64,
    /// Time since the most recently received packet.
    pub last_packet_age_ms: Option<u64>,
    /// Current adaptive playout delay in milliseconds.
    pub playout_delay_ms: u64,
    /// Number of frames synthesized after their playout deadline was missed.
    pub playout_underruns: u64,
    /// Number of frames generated by FEC, PLC, or delay expansion.
    pub concealed_frames: u64,
    /// Number of times the native speaker queue ran dry during active playout.
    pub speaker_underruns: u64,
}

#[cfg(feature = "webrtc-media")]
impl AudioSignalQuality {
    /// Classifies the metrics into a concise state for the call UI.
    #[must_use]
    pub fn state(self) -> AudioSignalState {
        if self.packets == 0 {
            return AudioSignalState::Waiting;
        }
        if self.last_packet_age_ms.is_some_and(|age| age > 1_500) {
            return AudioSignalState::Interrupted;
        }
        if self.packets < 5 {
            return AudioSignalState::Waiting;
        }
        if self.loss_percent >= 8.0 || self.jitter_ms >= 100.0 || self.decode_error_percent >= 5.0 {
            AudioSignalState::Poor
        } else if self.loss_percent >= 2.0
            || self.jitter_ms >= 45.0
            || self.decode_error_percent >= 1.0
        {
            AudioSignalState::Fair
        } else {
            AudioSignalState::Good
        }
    }
}

#[cfg(feature = "webrtc-media")]
/// Clears metrics when a different peer begins or the local user hangs up.
pub fn reset_audio_signal_quality() {
    REMOTE_PACKETS.store(0, Ordering::Relaxed);
    REMOTE_LOSS_PERCENT_BITS.store(0.0f64.to_bits(), Ordering::Relaxed);
    REMOTE_JITTER_MS_BITS.store(0.0f64.to_bits(), Ordering::Relaxed);
    REMOTE_DECODE_ERROR_PERCENT_BITS.store(0.0f64.to_bits(), Ordering::Relaxed);
    REMOTE_LAST_PACKET_MS.store(0, Ordering::Relaxed);
    REMOTE_PLAYOUT_DELAY_MS.store(0, Ordering::Relaxed);
    REMOTE_PLAYOUT_UNDERRUNS.store(0, Ordering::Relaxed);
    REMOTE_CONCEALED_FRAMES.store(0, Ordering::Relaxed);
    REMOTE_PLAYOUT_FRAMES.store(0, Ordering::Relaxed);
    crate::audio::reset_speaker_playout_metrics();
}

#[cfg(feature = "webrtc-media")]
/// Returns the latest incoming-audio measurements without taking a lock.
#[must_use]
pub fn audio_signal_quality() -> AudioSignalQuality {
    let last_packet_ms = REMOTE_LAST_PACKET_MS.load(Ordering::Relaxed);
    AudioSignalQuality {
        packets: REMOTE_PACKETS.load(Ordering::Relaxed),
        loss_percent: f64::from_bits(REMOTE_LOSS_PERCENT_BITS.load(Ordering::Relaxed)),
        jitter_ms: f64::from_bits(REMOTE_JITTER_MS_BITS.load(Ordering::Relaxed)),
        decode_error_percent: f64::from_bits(
            REMOTE_DECODE_ERROR_PERCENT_BITS.load(Ordering::Relaxed),
        ),
        last_packet_age_ms: (last_packet_ms != 0).then(|| now_ms().saturating_sub(last_packet_ms)),
        playout_delay_ms: REMOTE_PLAYOUT_DELAY_MS.load(Ordering::Relaxed),
        playout_underruns: REMOTE_PLAYOUT_UNDERRUNS.load(Ordering::Relaxed),
        concealed_frames: REMOTE_CONCEALED_FRAMES.load(Ordering::Relaxed),
        speaker_underruns: crate::audio::speaker_underruns(),
    }
}

#[cfg(feature = "webrtc-media")]
fn update_ema(metric: &AtomicU64, sample: f64, first_sample: bool) {
    let mut previous = metric.load(Ordering::Relaxed);
    loop {
        let previous_value = f64::from_bits(previous);
        let updated = if first_sample {
            sample
        } else {
            previous_value + 0.05 * (sample - previous_value)
        };
        match metric.compare_exchange_weak(
            previous,
            updated.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => previous = actual,
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn note_remote_arrival(jitter_ms: f64) {
    let previous_packets = REMOTE_PACKETS.fetch_add(1, Ordering::Relaxed);
    update_ema(
        &REMOTE_JITTER_MS_BITS,
        jitter_ms.clamp(0.0, 1_000.0),
        previous_packets == 0,
    );
    REMOTE_LAST_PACKET_MS.store(now_ms(), Ordering::Relaxed);
}

#[cfg(feature = "webrtc-media")]
fn note_remote_playout(missing: bool, decoded: bool, playout_delay_ms: u64) {
    let completed = REMOTE_PLAYOUT_FRAMES.fetch_add(1, Ordering::Relaxed);
    update_ema(
        &REMOTE_LOSS_PERCENT_BITS,
        if missing { 100.0 } else { 0.0 },
        completed == 0,
    );
    update_ema(
        &REMOTE_DECODE_ERROR_PERCENT_BITS,
        if decoded { 0.0 } else { 100.0 },
        completed == 0,
    );
    REMOTE_PLAYOUT_DELAY_MS.store(playout_delay_ms, Ordering::Relaxed);
    crate::audio::set_speaker_playout_delay(playout_delay_ms);
    if missing {
        REMOTE_PLAYOUT_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
        if decoded {
            REMOTE_CONCEALED_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn note_delay_expansion(playout_delay_ms: u64) {
    REMOTE_PLAYOUT_DELAY_MS.store(playout_delay_ms, Ordering::Relaxed);
    crate::audio::set_speaker_playout_delay(playout_delay_ms);
    REMOTE_CONCEALED_FRAMES.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "webrtc-media")]
#[derive(Debug)]
struct BufferedRtpPacket {
    sequence: u16,
    payload: Bytes,
}

#[cfg(feature = "webrtc-media")]
#[derive(Debug, Eq, PartialEq)]
enum JitterInsert {
    Accepted,
    Duplicate,
    Late,
    Resynchronized,
}

#[cfg(feature = "webrtc-media")]
struct AdaptiveJitterBuffer {
    packets: Vec<BufferedRtpPacket>,
    expected_sequence: Option<u16>,
    target_packets: usize,
    playout_packets: usize,
    frame_duration_ms: u64,
    started: bool,
    jitter_ms: f64,
}

#[cfg(feature = "webrtc-media")]
impl AdaptiveJitterBuffer {
    fn new(frame_duration_ms: u64) -> Self {
        let frame_duration_ms = frame_duration_ms.max(1);
        let target_packets = usize::try_from(MIN_PLAYOUT_DELAY_MS.div_ceil(frame_duration_ms))
            .unwrap_or(MAX_JITTER_PACKETS);
        Self {
            packets: Vec::with_capacity(MAX_JITTER_PACKETS),
            expected_sequence: None,
            target_packets,
            playout_packets: target_packets,
            frame_duration_ms,
            started: false,
            jitter_ms: 0.0,
        }
    }

    fn insert(&mut self, packet: BufferedRtpPacket, jitter_ms: f64) -> JitterInsert {
        self.update_target(jitter_ms);
        let Some(expected) = self.expected_sequence else {
            self.expected_sequence = Some(packet.sequence);
            self.packets.push(packet);
            return JitterInsert::Accepted;
        };
        let distance = packet.sequence.wrapping_sub(expected);
        if self
            .packets
            .iter()
            .any(|queued| queued.sequence == packet.sequence)
        {
            return JitterInsert::Duplicate;
        }
        if distance >= 0x8000 {
            let reverse_distance = expected.wrapping_sub(packet.sequence);
            if self.started || usize::from(reverse_distance) > MAX_JITTER_PACKETS {
                return JitterInsert::Late;
            }
            self.expected_sequence = Some(packet.sequence);
        } else if usize::from(distance) >= MAX_JITTER_PACKETS {
            self.packets.clear();
            self.expected_sequence = Some(packet.sequence);
            self.started = false;
            self.packets.push(packet);
            return JitterInsert::Resynchronized;
        }
        self.packets.push(packet);
        JitterInsert::Accepted
    }

    fn update_target(&mut self, jitter_ms: f64) {
        self.jitter_ms = if self.jitter_ms == 0.0 {
            jitter_ms
        } else {
            self.jitter_ms + 0.1 * (jitter_ms - self.jitter_ms)
        };
        let target_ms_float = (60.0 + self.jitter_ms * 2.5).ceil().clamp(60.0, 200.0);
        let target_ms = u64::try_from(
            std::time::Duration::from_secs_f64(target_ms_float / 1_000.0).as_millis(),
        )
        .unwrap_or(MAX_PLAYOUT_DELAY_MS);
        self.target_packets = usize::try_from(target_ms.div_ceil(self.frame_duration_ms))
            .unwrap_or(MAX_JITTER_PACKETS)
            .clamp(1, MAX_JITTER_PACKETS);
    }

    fn can_start(&self) -> bool {
        !self.started && self.packets.len() >= self.target_packets
    }

    fn start(&mut self) {
        // After a stall the buffered packets may start well ahead of the
        // sequence we were waiting for; jump to the nearest one instead of
        // concealing the whole gap one frame at a time.
        if let Some(expected) = self.expected_sequence {
            let nearest = self
                .packets
                .iter()
                .map(|packet| packet.sequence.wrapping_sub(expected))
                .filter(|distance| *distance < 0x8000)
                .min();
            if let Some(distance) = nearest {
                if usize::from(distance) > self.target_packets {
                    self.expected_sequence = Some(expected.wrapping_add(distance));
                }
            }
            let expected = self.expected_sequence.unwrap_or(expected);
            self.packets
                .retain(|packet| packet.sequence.wrapping_sub(expected) < 0x8000);
        }
        self.started = true;
        self.playout_packets = self.target_packets;
    }

    /// True when a packet newer than the expected one is already buffered, i.e.
    /// the expected packet is really lost rather than merely late or not sent.
    fn has_later_packet(&self) -> bool {
        let Some(expected) = self.expected_sequence else {
            return false;
        };
        self.packets.iter().any(|packet| {
            let distance = packet.sequence.wrapping_sub(expected);
            distance != 0 && distance < 0x8000
        })
    }

    /// Goes back to buffering after the stream stalled. The expected sequence
    /// number is kept, so packets that arrive later are still played.
    fn restart(&mut self) {
        self.started = false;
    }

    fn take_expected(&mut self) -> Option<BufferedRtpPacket> {
        let expected = self.expected_sequence?;
        let index = self
            .packets
            .iter()
            .position(|packet| packet.sequence == expected)?;
        let packet = self.packets.swap_remove(index);
        self.expected_sequence = Some(expected.wrapping_add(1));
        Some(packet)
    }

    fn next_payload(&self) -> Option<Bytes> {
        let sequence = self.expected_sequence?.wrapping_add(1);
        self.packets
            .iter()
            .find(|packet| packet.sequence == sequence)
            .map(|packet| packet.payload.clone())
    }

    fn skip_expected(&mut self) {
        if let Some(expected) = self.expected_sequence {
            self.expected_sequence = Some(expected.wrapping_add(1));
        }
    }

    fn can_expand_delay(&self) -> bool {
        self.playout_packets < self.target_packets
    }

    fn expand_delay(&mut self) {
        self.playout_packets = self.playout_packets.saturating_add(1);
    }

    fn playout_delay_ms(&self) -> u64 {
        u64::try_from(self.playout_packets)
            .unwrap_or(u64::MAX)
            .saturating_mul(self.frame_duration_ms)
    }

    fn startup_frames(&self) -> usize {
        self.playout_packets
    }
}

#[cfg(all(feature = "webrtc-media", feature = "opus-media"))]
fn configured_opus_encoder() -> Result<opus_pure::OpusEncoder, opus_pure::Error> {
    let mut encoder = opus_pure::OpusEncoder::new(48_000, 1, opus_pure::Application::Voip)?;
    encoder.bitrate_bps = 48_000;
    encoder.complexity = 10;
    encoder.rate_control = opus_pure::RateControl::ConstrainedVbr;
    encoder.use_inband_fec = true;
    encoder.packet_loss_perc = 5;
    Ok(encoder)
}

#[cfg(all(feature = "webrtc-media", feature = "opus-media"))]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpusTransmitProfile {
    bitrate_bps: i32,
    packet_loss_percent: i32,
    frame_samples: usize,
    fec: bool,
}

#[cfg(all(feature = "webrtc-media", feature = "opus-media"))]
#[allow(dead_code)]
impl OpusTransmitProfile {
    fn from_quality(quality: AudioSignalQuality) -> Self {
        match quality.state() {
            AudioSignalState::Waiting => Self {
                bitrate_bps: 48_000,
                packet_loss_percent: 5,
                frame_samples: OPUS_FRAME_SAMPLES,
                fec: true,
            },
            AudioSignalState::Good => Self {
                bitrate_bps: 56_000,
                packet_loss_percent: 5,
                frame_samples: OPUS_FRAME_SAMPLES,
                fec: true,
            },
            AudioSignalState::Fair => Self {
                bitrate_bps: 44_000,
                packet_loss_percent: 10,
                frame_samples: OPUS_FRAME_SAMPLES,
                fec: true,
            },
            AudioSignalState::Poor | AudioSignalState::Interrupted => Self {
                bitrate_bps: 36_000,
                packet_loss_percent: 20,
                frame_samples: OPUS_FRAME_SAMPLES,
                fec: true,
            },
        }
    }

    fn apply(self, encoder: &mut opus_pure::OpusEncoder) {
        encoder.bitrate_bps = self.bitrate_bps;
        encoder.packet_loss_perc = self.packet_loss_percent;
        encoder.use_inband_fec = self.fec;
    }
}

#[cfg(all(feature = "webrtc-media", feature = "opus-media"))]
#[allow(dead_code)]
fn encode_opus_frame(
    encoder: &mut opus_pure::OpusEncoder,
    pcm: &[i16],
) -> Result<Vec<u8>, opus_pure::Error> {
    let mut packet = vec![0; 1_276];
    let packet_len = encoder.encode_s16(pcm, pcm.len(), &mut packet)?;
    packet.truncate(packet_len);
    Ok(packet)
}

#[cfg(feature = "webrtc-media")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioCodec {
    G711(u8),
    #[cfg(feature = "opus-media")]
    Opus,
}

#[cfg(feature = "webrtc-media")]
#[allow(dead_code)]
enum OutgoingAudioFrame {
    G711 {
        payload_type: u8,
        payload: Vec<u8>,
    },
    #[cfg(feature = "opus-media")]
    Opus {
        payload: Vec<u8>,
        frame_samples: u32,
    },
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
struct CapturedMicrophoneAudio {
    pcm: Vec<i16>,
    sample_rate: u32,
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
enum MicrophoneWorkerMessage {
    Audio(CapturedMicrophoneAudio),
    Shutdown,
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
struct MicrophoneEncoderState {
    codec: AudioCodec,
    processor: crate::audio::MicrophoneProcessor,
    muted: Arc<AtomicBool>,
    session_recorder: Option<crate::session_recorder::StereoSessionRecorder>,
    resampler: crate::audio::LinearResampler,
    resample_buffer: Vec<i16>,
    pcmu_buffer: PcmuFrameAssembler,
    g711_buffer: Vec<u8>,
    #[cfg(feature = "opus-media")]
    opus_buffer: Vec<i16>,
    #[cfg(feature = "opus-media")]
    opus_encoder: Option<opus_pure::OpusEncoder>,
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
impl MicrophoneEncoderState {
    fn process(
        &mut self,
        pcm: &mut [i16],
        sample_rate: u32,
        audio_tx: &mpsc::Sender<OutgoingAudioFrame>,
    ) {
        self.processor.process(pcm, sample_rate);
        crate::audio::record_microphone_level(pcm);
        if self.muted.load(Ordering::Relaxed) {
            pcm.fill(0);
        }
        if let Some(recorder) = self.session_recorder.as_ref() {
            recorder.record_local(pcm, sample_rate);
        }

        #[cfg(feature = "opus-media")]
        if self.codec == AudioCodec::Opus {
            self.resampler
                .resample_into(pcm, sample_rate, 48_000, &mut self.resample_buffer);
            self.opus_buffer.extend_from_slice(&self.resample_buffer);
            let profile = OpusTransmitProfile::from_quality(audio_signal_quality());
            let mut frame = [0i16; OPUS_MAX_FRAME_SAMPLES];
            while self.opus_buffer.len() >= profile.frame_samples {
                frame[..profile.frame_samples]
                    .copy_from_slice(&self.opus_buffer[..profile.frame_samples]);
                self.opus_buffer.drain(..profile.frame_samples);
                let payload = self.opus_encoder.as_mut().and_then(|encoder| {
                    profile.apply(encoder);
                    encode_opus_frame(encoder, &frame[..profile.frame_samples]).ok()
                });
                if let Some(payload) = payload {
                    let frame_samples = u32::try_from(profile.frame_samples).unwrap_or(960);
                    let _ = audio_tx.try_send(OutgoingAudioFrame::Opus {
                        payload,
                        frame_samples,
                    });
                }
            }
            return;
        }

        self.resampler.resample_into(
            pcm,
            sample_rate,
            crate::audio::VOICE_SAMPLE_RATE,
            &mut self.resample_buffer,
        );
        let payload_type = match self.codec {
            AudioCodec::G711(payload_type) => payload_type,
            #[cfg(feature = "opus-media")]
            AudioCodec::Opus => return,
        };
        crate::voice::g711_encode_into(payload_type, &self.resample_buffer, &mut self.g711_buffer);
        let frames = self.pcmu_buffer.push(&self.g711_buffer);
        if !frames.is_empty() {
            let _ = audio_tx.try_send(OutgoingAudioFrame::G711 {
                payload_type,
                payload: frames,
            });
        }
    }
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
struct MicrophoneEncodingWorker {
    sender: sync_mpsc::SyncSender<MicrophoneWorkerMessage>,
    recycle_sender: sync_mpsc::SyncSender<Vec<i16>>,
    recycled: Mutex<sync_mpsc::Receiver<Vec<i16>>>,
    shutdown: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
impl MicrophoneEncodingWorker {
    fn spawn(
        mut state: MicrophoneEncoderState,
        audio_tx: mpsc::Sender<OutgoingAudioFrame>,
    ) -> Result<Arc<Self>, String> {
        let (sender, receiver) = sync_mpsc::sync_channel(MICROPHONE_CAPTURE_BUFFERS);
        let (recycle_sender, recycled) = sync_mpsc::sync_channel(MICROPHONE_CAPTURE_BUFFERS);
        for _ in 0..MICROPHONE_CAPTURE_BUFFERS {
            let _ = recycle_sender.try_send(Vec::with_capacity(MICROPHONE_CAPTURE_BUFFER_SAMPLES));
        }
        let thread_recycle_sender = recycle_sender.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("nektome-microphone-encoder".into())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    let Ok(message) = receiver.recv() else {
                        break;
                    };
                    match message {
                        MicrophoneWorkerMessage::Audio(mut audio) => {
                            state.process(&mut audio.pcm, audio.sample_rate, &audio_tx);
                            audio.pcm.clear();
                            let _ = thread_recycle_sender.try_send(audio.pcm);
                        }
                        MicrophoneWorkerMessage::Shutdown => break,
                    }
                }
            })
            .map_err(|error| format!("could not start microphone encoder: {error}"))?;
        Ok(Arc::new(Self {
            sender,
            recycle_sender,
            recycled: Mutex::new(recycled),
            shutdown,
            thread: Mutex::new(Some(thread)),
        }))
    }

    fn enqueue(&self, pcm: &[i16], sample_rate: u32) {
        for samples in pcm.chunks(MICROPHONE_CAPTURE_BUFFER_SAMPLES) {
            let buffer = self
                .recycled
                .try_lock()
                .ok()
                .and_then(|recycled| recycled.try_recv().ok());
            let mut buffer = buffer.unwrap_or_else(|| Vec::with_capacity(samples.len()));
            buffer.extend_from_slice(samples);
            let message = MicrophoneWorkerMessage::Audio(CapturedMicrophoneAudio {
                pcm: buffer,
                sample_rate,
            });
            if let Err(error) = self.sender.try_send(message) {
                let message = match error {
                    sync_mpsc::TrySendError::Full(message)
                    | sync_mpsc::TrySendError::Disconnected(message) => message,
                };
                let MicrophoneWorkerMessage::Audio(mut audio) = message else {
                    return;
                };
                audio.pcm.clear();
                let _ = self.recycle_sender.try_send(audio.pcm);
                return;
            }
        }
    }
}

#[cfg(all(feature = "webrtc-media", feature = "cpal-audio"))]
impl Drop for MicrophoneEncodingWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.send(MicrophoneWorkerMessage::Shutdown);
        if let Some(thread) = self.thread.get_mut().ok().and_then(Option::take) {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "webrtc-media")]
impl AudioCodec {
    fn from_environment() -> Self {
        Self::from_name(std::env::var("NEKTOME_AUDIO_CODEC").ok().as_deref())
    }

    fn from_name(value: Option<&str>) -> Self {
        let value = value.map(|value| value.trim().to_ascii_lowercase());
        match value.as_deref() {
            Some("pcmu") | Some("mulaw") | Some("mu-law") => Self::G711(0),
            Some("pcma") | Some("alaw") | Some("a-law") => Self::G711(8),
            #[cfg(feature = "opus-media")]
            Some("opus") | Some("opus/48000") => Self::Opus,
            #[cfg(feature = "opus-media")]
            _ => Self::Opus,
            #[cfg(not(feature = "opus-media"))]
            _ => Self::G711(0),
        }
    }

    fn payload_type(self) -> u8 {
        match self {
            Self::G711(payload_type) => payload_type,
            #[cfg(feature = "opus-media")]
            Self::Opus => 111,
        }
    }

    fn sample_rate(self) -> u32 {
        match self {
            Self::G711(_) => 8_000,
            #[cfg(feature = "opus-media")]
            Self::Opus => 48_000,
        }
    }

    fn frame_samples(self) -> usize {
        match self {
            Self::G711(_) => 160,
            #[cfg(feature = "opus-media")]
            Self::Opus => OPUS_FRAME_SAMPLES,
        }
    }
}

#[cfg(all(feature = "webrtc-media", any(feature = "cpal-audio", test)))]
#[derive(Default)]
struct PcmuFrameAssembler {
    buffer: Vec<u8>,
}

#[cfg(all(feature = "webrtc-media", any(feature = "cpal-audio", test)))]
impl PcmuFrameAssembler {
    fn push(&mut self, payload: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(payload);
        let drain_len = self.buffer.len() / PCMU_FRAME_BYTES * PCMU_FRAME_BYTES;
        let frames = self.buffer[..drain_len].to_vec();
        self.buffer.drain(..drain_len);
        frames
    }
}

#[cfg(feature = "webrtc-media")]
type LocalCandidateCallback = Arc<dyn Fn(String, i32, String) + Send + Sync>;

#[cfg(feature = "webrtc-media")]
type IceStateCallback = Arc<dyn Fn(String) + Send + Sync>;

#[cfg(feature = "webrtc-media")]
#[cfg(feature = "webrtc-media")]
struct IncomingAudioTask {
    ssrc: Option<u32>,
    handle: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "webrtc-media")]
#[derive(Clone)]
struct WebRtcEventHandler {
    local_candidate: Arc<Mutex<Option<LocalCandidateCallback>>>,
    ice_state: Arc<Mutex<Option<IceStateCallback>>>,
    sink: Option<Arc<dyn crate::audio::AudioSink>>,
    remote_started: RemoteStarted,
    shutdown_rx: watch::Receiver<bool>,
    incoming_audio_task: Arc<Mutex<Option<IncomingAudioTask>>>,
}

#[cfg(feature = "webrtc-media")]
#[async_trait::async_trait]
impl PeerConnectionEventHandler for WebRtcEventHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Some(callback) = self
            .local_candidate
            .lock()
            .ok()
            .and_then(|handler| handler.as_ref().cloned())
        else {
            return;
        };
        if let Ok(init) = event.candidate.to_json() {
            callback(
                init.sdp_mid.unwrap_or_else(|| "0".into()),
                i32::from(init.sdp_mline_index.unwrap_or(0)),
                init.candidate,
            );
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        if let Some(callback) = self
            .ice_state
            .lock()
            .ok()
            .and_then(|handler| handler.as_ref().cloned())
        {
            callback(state.to_string());
        }
    }

    async fn on_ice_gathering_state_change(&self, _state: RTCIceGatheringState) {}

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(sink) = self.sink.as_ref() {
            let ssrc = track.ssrcs().await.first().copied();
            if let Ok(mut task_guard) = self.incoming_audio_task.lock() {
                if let Some(current) = task_guard.as_ref() {
                    if ssrc.is_some() && current.ssrc == ssrc {
                        return;
                    }
                }
                if let Some(old_task) = task_guard.take() {
                    old_task.handle.abort();
                }
                let new_task = tokio::spawn(run_incoming_audio(
                    track,
                    Arc::clone(sink),
                    Arc::clone(&self.remote_started),
                    self.shutdown_rx.clone(),
                ));
                *task_guard = Some(IncomingAudioTask {
                    ssrc,
                    handle: new_task,
                });
            }
        }
    }
}

#[cfg(feature = "webrtc-media")]
pub struct WebRtcPeer {
    connection: Arc<dyn PeerConnection>,
    audio_track: Arc<TrackLocalStaticRTP>,
    sequence: Arc<AtomicU16>,
    timestamp: Arc<AtomicU32>,
    last_audio_write_ms: Arc<AtomicU64>,
    muted: Arc<AtomicBool>,
    codec: AudioCodec,
    audio_shutdown: watch::Sender<bool>,
    rtp_write_lock: Arc<AsyncMutex<()>>,
    remote_started: RemoteStarted,
    local_candidate: Arc<Mutex<Option<LocalCandidateCallback>>>,
    ice_state: Arc<Mutex<Option<IceStateCallback>>>,
    session_recorder: Mutex<Option<crate::session_recorder::StereoSessionRecorder>>,
    _turn_forwarder: Option<crate::turn_forwarder::TurnTcpForwarder>,
    incoming_audio_task: Arc<Mutex<Option<IncomingAudioTask>>>,
}

#[cfg(feature = "webrtc-media")]
impl WebRtcPeer {
    pub async fn new(
        ice_servers: &[crate::voice::IceServer],
        sink: Option<Arc<dyn crate::audio::AudioSink>>,
        session_recorder: Option<crate::session_recorder::StereoSessionRecorder>,
        proxy: Option<&str>,
    ) -> Result<Self, webrtc::error::Error> {
        let (ice_servers, turn_forwarder) =
            crate::turn_forwarder::TurnTcpForwarder::prepare(ice_servers, proxy).await;
        let mut media = MediaEngine::default();
        media.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;
        let ice_servers = ice_servers.iter().map(rtc_ice_server).collect();
        let codec = AudioCodec::from_environment();
        #[cfg(feature = "opus-media")]
        if codec == AudioCodec::Opus && configured_opus_encoder().is_err() {
            return Err(webrtc::error::Error::ErrUnknownType);
        }
        let track_codec = RTCRtpCodec {
            mime_type: match codec {
                AudioCodec::G711(8) => MIME_TYPE_PCMA.into(),
                AudioCodec::G711(_) => MIME_TYPE_PCMU.into(),
                #[cfg(feature = "opus-media")]
                AudioCodec::Opus => MIME_TYPE_OPUS.into(),
            },
            clock_rate: codec.sample_rate(),
            channels: 1,
            sdp_fmtp_line: match codec {
                #[cfg(feature = "opus-media")]
                AudioCodec::Opus => "minptime=10;useinbandfec=1".into(),
                AudioCodec::G711(_) => String::new(),
            },
            rtcp_feedback: Vec::new(),
        };
        let audio_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "nektome".into(),
            "audio".into(),
            "audio".into(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(1),
                    ..Default::default()
                },
                codec: track_codec,
                ..Default::default()
            }],
        )));
        let remote_started = Arc::new(Mutex::new(None));
        let local_candidate = Arc::new(Mutex::new(None));
        let ice_state = Arc::new(Mutex::new(None));
        let (audio_shutdown, shutdown_rx) = watch::channel(false);
        let incoming_audio_task = Arc::new(Mutex::new(None));
        let handler = Arc::new(WebRtcEventHandler {
            local_candidate: Arc::clone(&local_candidate),
            ice_state: Arc::clone(&ice_state),
            sink,
            remote_started: Arc::clone(&remote_started),
            shutdown_rx: shutdown_rx.clone(),
            incoming_audio_task: Arc::clone(&incoming_audio_task),
        });
        let connection: Arc<dyn PeerConnection> = Arc::new(
            Box::pin(
                PeerConnectionBuilder::new()
                    .with_configuration(
                        RTCConfigurationBuilder::new()
                            .with_ice_servers(ice_servers)
                            .build(),
                    )
                    .with_media_engine(media)
                    .with_interceptor_registry(registry)
                    .with_handler(handler)
                    .with_udp_addrs(vec!["0.0.0.0:0"])
                    .build(),
            )
            .await?,
        );
        connection
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
            .await?;
        let sequence = Arc::new(AtomicU16::new(0));
        let timestamp = Arc::new(AtomicU32::new(0));
        let last_audio_write_ms = Arc::new(AtomicU64::new(now_ms()));
        let muted = Arc::new(AtomicBool::new(false));
        let silence_track = Arc::clone(&audio_track);
        let silence_sequence = Arc::clone(&sequence);
        let silence_timestamp = Arc::clone(&timestamp);
        let silence_codec = codec;
        let silence_last_write = Arc::clone(&last_audio_write_ms);
        let rtp_write_lock = Arc::new(AsyncMutex::new(()));
        let silence_write_lock = Arc::clone(&rtp_write_lock);
        let mut silence_shutdown_rx = shutdown_rx;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
            #[cfg(feature = "opus-media")]
            let mut silence_encoder = if silence_codec == AudioCodec::Opus {
                configured_opus_encoder().ok()
            } else {
                None
            };
            let silence = match silence_codec {
                AudioCodec::G711(payload_type) => {
                    crate::voice::g711_encode(payload_type, &[0; 160])
                }
                #[cfg(feature = "opus-media")]
                AudioCodec::Opus => silence_encoder
                    .as_mut()
                    .and_then(|encoder| encode_opus_frame(encoder, &[0; OPUS_FRAME_SAMPLES]).ok())
                    .unwrap_or_default(),
            };
            let silence = Bytes::from(silence);
            loop {
                tokio::select! {
                    _ = silence_shutdown_rx.changed() => break,
                    _ = interval.tick() => {
                        if now_ms().saturating_sub(silence_last_write.load(Ordering::Relaxed)) < 60 {
                            continue;
                        }
                        let _write_guard = silence_write_lock.lock().await;
                        let _ = write_rtp_packet(
                            &silence_track,
                            &silence_sequence,
                            &silence_timestamp,
                            silence_codec.payload_type(),
                            silence_codec.frame_samples() as u32,
                            silence.clone(),
                        ).await;
                    }
                }
            }
        });
        Ok(Self {
            connection,
            audio_track,
            sequence,
            timestamp,
            last_audio_write_ms,
            muted,
            codec,
            audio_shutdown,
            rtp_write_lock,
            remote_started,
            local_candidate,
            ice_state,
            session_recorder: Mutex::new(session_recorder),
            _turn_forwarder: turn_forwarder,
            incoming_audio_task,
        })
    }

    #[cfg(feature = "cpal-audio")]
    /// Creates a microphone capture whose processing and encoding run on a
    /// bounded dedicated worker.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected codec cannot be configured or the
    /// encoder worker thread cannot be started.
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    pub fn microphone(
        self: &Arc<Self>,
        microphone_gain_percent: f32,
        microphone_dsp_enabled: bool,
        microphone_warmth_enabled: bool,
        saturation_drive: f32,
        saturation_asymmetry: f32,
        saturation_bias: f32,
        presence_boost_db: f32,
        proximity_gain_db: f32,
        air_boost_db: f32,
        voice_changer_enabled: bool,
        voice_changer_model: &str,
        voice_changer_pitch: f32,
        voice_changer_wet: f32,
        microphone_noise_suppression_enabled: bool,
        echo_cancellation_enabled: bool,
        noise_gate_threshold: f32,
        compressor_threshold: f32,
        compressor_ratio: f32,
        makeup_gain: f32,
        session_recorder: Option<crate::session_recorder::StereoSessionRecorder>,
    ) -> Result<crate::audio::CpalMicrophone, String> {
        let muted = Arc::clone(&self.muted);
        let handle = tokio::runtime::Handle::current();
        let codec = self.codec;
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(32);
        let writer_peer = Arc::clone(self);
        handle.spawn(async move {
            while let Some(frame) = audio_rx.recv().await {
                match frame {
                    OutgoingAudioFrame::G711 {
                        payload_type,
                        payload,
                    } => {
                        let _ = writer_peer.write_g711(payload_type, payload).await;
                    }
                    #[cfg(feature = "opus-media")]
                    OutgoingAudioFrame::Opus {
                        payload,
                        frame_samples,
                    } => {
                        let _ = writer_peer.write_opus(payload, frame_samples).await;
                    }
                }
            }
        });
        let mut microphone_processor = crate::audio::MicrophoneProcessor::new(48_000);
        microphone_processor.gain_percent = microphone_gain_percent;
        microphone_processor.configure_dynamics(
            noise_gate_threshold,
            compressor_threshold,
            compressor_ratio,
            makeup_gain,
        );
        microphone_processor.configure_warmth(
            microphone_warmth_enabled,
            saturation_drive,
            saturation_asymmetry,
            saturation_bias,
        );
        microphone_processor.configure_presence(presence_boost_db);
        microphone_processor.configure_proximity(proximity_gain_db);
        microphone_processor.configure_air(air_boost_db);
        microphone_processor.configure_voice_changer(
            voice_changer_enabled,
            voice_changer_model,
            voice_changer_pitch,
            voice_changer_wet,
        );
        microphone_processor.dsp_enabled = microphone_dsp_enabled;
        microphone_processor.noise_suppression_enabled = microphone_noise_suppression_enabled;
        microphone_processor.echo_cancellation_enabled = echo_cancellation_enabled;
        let state = MicrophoneEncoderState {
            codec,
            processor: microphone_processor,
            muted,
            session_recorder,
            resampler: crate::audio::LinearResampler::new(),
            resample_buffer: Vec::with_capacity(MICROPHONE_CAPTURE_BUFFER_SAMPLES),
            pcmu_buffer: PcmuFrameAssembler::default(),
            g711_buffer: Vec::with_capacity(MICROPHONE_CAPTURE_BUFFER_SAMPLES),
            #[cfg(feature = "opus-media")]
            opus_buffer: Vec::with_capacity(OPUS_MAX_FRAME_SAMPLES * 2),
            #[cfg(feature = "opus-media")]
            opus_encoder: (codec == AudioCodec::Opus)
                .then(configured_opus_encoder)
                .transpose()
                .map_err(|error| format!("could not configure Opus encoder: {error}"))?,
        };
        let worker = MicrophoneEncodingWorker::spawn(state, audio_tx)?;
        Ok(crate::audio::CpalMicrophone::new(
            move |pcm, sample_rate| worker.enqueue(pcm, sample_rate),
        ))
    }

    pub async fn create_offer(&self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let offer = self.connection.create_offer(None).await?;
        self.connection.set_local_description(offer).await?;
        Ok(self
            .connection
            .pending_local_description()
            .await
            .or(self.connection.local_description().await)
            .ok_or_else(|| std::io::Error::other("local offer SDP is missing"))?
            .sdp)
    }

    pub async fn accept_offer(
        &self,
        sdp: String,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.connection
            .set_remote_description(RTCSessionDescription::offer(sdp)?)
            .await?;
        let answer = self.connection.create_answer(None).await?;
        self.connection.set_local_description(answer).await?;
        Ok(self
            .connection
            .pending_local_description()
            .await
            .or(self.connection.local_description().await)
            .ok_or_else(|| std::io::Error::other("local answer SDP is missing"))?
            .sdp)
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<(), webrtc::error::Error> {
        self.connection
            .set_remote_description(RTCSessionDescription::answer(sdp)?)
            .await
    }

    pub async fn add_remote_candidate(
        &self,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
    ) -> Result<(), webrtc::error::Error> {
        self.connection
            .add_ice_candidate(RTCIceCandidateInit {
                candidate,
                sdp_mid,
                sdp_mline_index: sdp_m_line_index,
                ..Default::default()
            })
            .await
    }

    pub async fn close(&self) -> Result<(), webrtc::error::Error> {
        let _ = self.audio_shutdown.send(true);
        if let Ok(mut task_guard) = self.incoming_audio_task.lock()
            && let Some(task) = task_guard.take()
        {
            task.handle.abort();
        }
        reset_audio_signal_quality();
        let recorder = self
            .session_recorder
            .lock()
            .ok()
            .and_then(|mut recorder| recorder.take());
        if let Some(recorder) = recorder {
            let _ = tokio::task::spawn_blocking(move || recorder.finish()).await;
        }
        self.connection.close().await
    }

    pub async fn write_pcmu(&self, payload: Vec<u8>) -> Result<(), webrtc::error::Error> {
        self.write_g711(0, payload).await
    }

    pub async fn write_g711(
        &self,
        payload_type: u8,
        payload: Vec<u8>,
    ) -> Result<(), webrtc::error::Error> {
        if payload.is_empty() {
            return Ok(());
        }
        let _write_guard = self.rtp_write_lock.lock().await;
        let mut result = Ok(());
        let mut payload = Bytes::from(payload);
        while !payload.is_empty() {
            let frame = payload.split_to(payload.len().min(PCMU_FRAME_BYTES));
            result = write_rtp_packet(
                &self.audio_track,
                &self.sequence,
                &self.timestamp,
                payload_type,
                160,
                frame,
            )
            .await;
            if result.is_err() {
                break;
            }
        }
        if result.is_ok() {
            self.last_audio_write_ms.store(now_ms(), Ordering::Relaxed);
        }
        result
    }

    #[cfg(feature = "opus-media")]
    pub async fn write_opus(
        &self,
        payload: Vec<u8>,
        frame_samples: u32,
    ) -> Result<(), webrtc::error::Error> {
        if payload.is_empty() {
            return Ok(());
        }
        let _write_guard = self.rtp_write_lock.lock().await;
        let result = write_rtp_packet(
            &self.audio_track,
            &self.sequence,
            &self.timestamp,
            111,
            frame_samples,
            Bytes::from(payload),
        )
        .await;
        if result.is_ok() {
            self.last_audio_write_ms.store(now_ms(), Ordering::Relaxed);
        }
        result
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn codec_name(&self) -> &'static str {
        match self.codec {
            AudioCodec::G711(0) => "PCMU",
            AudioCodec::G711(8) => "PCMA",
            AudioCodec::G711(_) => "G.711",
            #[cfg(feature = "opus-media")]
            AudioCodec::Opus => "Opus",
        }
    }

    pub fn on_local_candidate(&self, callback: Arc<dyn Fn(String, i32, String) + Send + Sync>) {
        if let Ok(mut handler) = self.local_candidate.lock() {
            *handler = Some(callback);
        }
    }

    pub fn on_ice_connection_state_change(&self, callback: Arc<dyn Fn(String) + Send + Sync>) {
        if let Ok(mut handler) = self.ice_state.lock() {
            *handler = Some(callback);
        }
    }

    pub fn on_remote_audio_started(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut handler) = self.remote_started.lock() {
            *handler = Some(callback);
        }
    }
}

#[cfg(feature = "webrtc-media")]
impl Drop for WebRtcPeer {
    fn drop(&mut self) {
        let _ = self.audio_shutdown.send(true);
        if let Ok(mut task_guard) = self.incoming_audio_task.lock()
            && let Some(task) = task_guard.take()
        {
            task.handle.abort();
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(feature = "webrtc-media")]
async fn write_rtp_packet(
    track: &TrackLocalStaticRTP,
    sequence: &AtomicU16,
    timestamp: &AtomicU32,
    payload_type: u8,
    timestamp_step: u32,
    payload: Bytes,
) -> Result<(), webrtc::error::Error> {
    let packet = rtc::rtp::Packet {
        header: rtc::rtp::Header {
            version: 2,
            payload_type,
            sequence_number: sequence.fetch_add(1, Ordering::Relaxed),
            timestamp: timestamp.fetch_add(timestamp_step, Ordering::Relaxed),
            ssrc: 1,
            ..Default::default()
        },
        payload,
    };
    track.write_rtp_with_extensions(packet, &[]).await
}

#[cfg(feature = "webrtc-media")]
struct ReceivedRtpPacket {
    packet: rtc::rtp::Packet,
    arrival: Instant,
}

#[cfg(feature = "webrtc-media")]
#[derive(Default)]
struct ArrivalJitter {
    latest: Option<(u16, u32, Instant)>,
}

#[cfg(feature = "webrtc-media")]
impl ArrivalJitter {
    fn observe(&mut self, sequence: u16, timestamp: u32, arrival: Instant, clock_rate: u32) -> f64 {
        let Some(previous) = self.latest else {
            self.latest = Some((sequence, timestamp, arrival));
            return 0.0;
        };
        let distance = sequence.wrapping_sub(previous.0);
        if distance == 0 || distance >= 0x8000 {
            return 0.0;
        }
        let expected_ms =
            f64::from(timestamp.wrapping_sub(previous.1)) * 1_000.0 / f64::from(clock_rate.max(1));
        let arrival_ms = arrival.duration_since(previous.2).as_secs_f64() * 1_000.0;
        self.latest = Some((sequence, timestamp, arrival));
        (arrival_ms - expected_ms).abs()
    }
}

#[cfg(feature = "webrtc-media")]
enum IncomingAudioDecoder {
    #[cfg(feature = "opus-media")]
    Opus {
        decoder: Box<opus_pure::OpusDecoder>,
        pcm: Vec<i16>,
        last_frame: Vec<i16>,
        extension: Vec<i16>,
        frame_samples: usize,
        fade_in: bool,
    },
    G711 {
        payload_type: u8,
        pcm: Vec<i16>,
        last_frame: Vec<i16>,
        extension: Vec<i16>,
        frame_samples: usize,
        fade_in: bool,
    },
}

#[cfg(feature = "webrtc-media")]
impl IncomingAudioDecoder {
    fn new(is_opus: bool, payload_type: u8, first_payload: &[u8]) -> Self {
        #[cfg(feature = "opus-media")]
        if is_opus || payload_type == 111 {
            let frame_samples = opus_pure::packet::samples_48k(first_payload)
                .unwrap_or(OPUS_FRAME_SAMPLES)
                .clamp(120, opus_pure::MAX_PACKET_SAMPLES);
            let decoder = opus_pure::OpusDecoder::new(48_000, 1)
                .expect("48 kHz mono is always a valid Opus decoder configuration");
            return Self::Opus {
                decoder: Box::new(decoder),
                pcm: vec![0; opus_pure::MAX_PACKET_SAMPLES],
                last_frame: Vec::with_capacity(opus_pure::MAX_PACKET_SAMPLES),
                extension: vec![0; frame_samples],
                frame_samples,
                fade_in: false,
            };
        }
        let frame_samples = if first_payload.is_empty() {
            PCMU_FRAME_BYTES
        } else {
            first_payload.len()
        };
        Self::G711 {
            payload_type,
            pcm: Vec::with_capacity(frame_samples),
            last_frame: Vec::with_capacity(frame_samples),
            extension: vec![0; frame_samples],
            frame_samples,
            fade_in: false,
        }
    }

    fn frame_duration_ms(&self) -> u64 {
        let (samples, sample_rate) = match self {
            #[cfg(feature = "opus-media")]
            Self::Opus { frame_samples, .. } => (*frame_samples, 48_000u64),
            Self::G711 { frame_samples, .. } => (*frame_samples, 8_000u64),
        };
        u64::try_from(samples)
            .unwrap_or(u64::MAX)
            .saturating_mul(1_000)
            .div_ceil(sample_rate)
            .max(1)
    }

    fn write_packet(&mut self, payload: &[u8], sink: &dyn crate::audio::AudioSink) -> bool {
        match self {
            #[cfg(feature = "opus-media")]
            Self::Opus {
                decoder,
                pcm,
                last_frame,
                extension,
                frame_samples,
                fade_in,
            } => {
                pcm.resize(opus_pure::MAX_PACKET_SAMPLES, 0);
                let Ok(samples) = decoder.decode_s16(payload, opus_pure::MAX_PACKET_SAMPLES, pcm)
                else {
                    crate::audio::audio_debug_log(|| {
                        format!("opus decode error, payload {} bytes", payload.len())
                    });
                    // Corrupt packet: conceal from decoder state instead of
                    // cutting to digital silence (an audible click).
                    let frame = *frame_samples;
                    if let Ok(concealed) = decoder.decode_s16(&[], frame, pcm) {
                        sink.write(&pcm[..concealed], 48_000);
                        last_frame.clear();
                        last_frame.extend_from_slice(&pcm[..concealed]);
                    } else {
                        synthesize_extension(last_frame, extension, frame);
                        sink.write(extension, 48_000);
                        last_frame.clear();
                        last_frame.extend_from_slice(extension);
                        *fade_in = true;
                    }
                    return false;
                };
                if *fade_in {
                    fade_in_head(&mut pcm[..samples], 48_000);
                    *fade_in = false;
                }
                sink.write(&pcm[..samples], 48_000);
                last_frame.clear();
                last_frame.extend_from_slice(&pcm[..samples]);
                *frame_samples = samples;
                true
            }
            Self::G711 {
                payload_type,
                pcm,
                last_frame,
                frame_samples,
                fade_in,
                ..
            } => {
                crate::voice::decode_into(*payload_type, payload, pcm);
                if pcm.is_empty() {
                    pcm.resize(*frame_samples, 0);
                    sink.write(pcm, crate::audio::VOICE_SAMPLE_RATE);
                    return false;
                }
                if *fade_in {
                    fade_in_head(pcm, crate::audio::VOICE_SAMPLE_RATE);
                    *fade_in = false;
                }
                sink.write(pcm, crate::audio::VOICE_SAMPLE_RATE);
                last_frame.clear();
                last_frame.extend_from_slice(pcm);
                *frame_samples = pcm.len();
                true
            }
        }
    }

    fn write_concealment(
        &mut self,
        following_payload: Option<&[u8]>,
        sink: &dyn crate::audio::AudioSink,
    ) -> bool {
        match self {
            #[cfg(feature = "opus-media")]
            Self::Opus {
                decoder,
                pcm,
                last_frame,
                extension,
                frame_samples,
                fade_in,
            } => {
                pcm.resize(opus_pure::MAX_PACKET_SAMPLES, 0);
                let decoded = if let Some(payload) = following_payload {
                    decoder.decode_fec_s16(payload, *frame_samples, pcm)
                } else {
                    decoder.decode_s16(&[], *frame_samples, pcm)
                };
                let Ok(samples) = decoded else {
                    let frame = *frame_samples;
                    synthesize_extension(last_frame, extension, frame);
                    sink.write(extension, 48_000);
                    last_frame.clear();
                    last_frame.extend_from_slice(extension);
                    *fade_in = true;
                    return false;
                };
                // A concealment frame counts as consuming any pending fade-in so
                // that the next real packet does not re-apply it.
                *fade_in = false;
                sink.write(&pcm[..samples], 48_000);
                last_frame.clear();
                last_frame.extend_from_slice(&pcm[..samples]);
                true
            }
            Self::G711 {
                last_frame,
                extension,
                frame_samples,
                ..
            } => {
                synthesize_extension(last_frame, extension, *frame_samples);
                sink.write(extension, crate::audio::VOICE_SAMPLE_RATE);
                last_frame.clear();
                last_frame.extend_from_slice(extension);
                true
            }
        }
    }

    fn write_delay_extension(&mut self, sink: &dyn crate::audio::AudioSink) {
        match self {
            #[cfg(feature = "opus-media")]
            Self::Opus {
                decoder,
                pcm,
                last_frame,
                extension,
                frame_samples,
                fade_in,
            } => {
                // Extend the delay with Opus PLC so the waveform continues
                // naturally; a fade-to-silence frame is audible as a dropout.
                pcm.resize(opus_pure::MAX_PACKET_SAMPLES, 0);
                if let Ok(samples) = decoder.decode_s16(&[], *frame_samples, pcm) {
                    sink.write(&pcm[..samples], 48_000);
                    last_frame.clear();
                    last_frame.extend_from_slice(&pcm[..samples]);
                    *fade_in = false;
                } else {
                    synthesize_extension(last_frame, extension, *frame_samples);
                    sink.write(extension, 48_000);
                    *fade_in = true;
                }
            }
            Self::G711 {
                last_frame,
                extension,
                frame_samples,
                fade_in,
                ..
            } => {
                synthesize_extension(last_frame, extension, *frame_samples);
                sink.write(extension, crate::audio::VOICE_SAMPLE_RATE);
                *fade_in = true;
            }
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn fade_in_head(pcm: &mut [i16], sample_rate: u32) {
    let fade_samples = pcm
        .len()
        .min(usize::try_from(sample_rate / 200).unwrap_or(0));
    let denominator = i32::try_from(fade_samples.max(1)).unwrap_or(i32::MAX);
    for (index, sample) in pcm.iter_mut().take(fade_samples).enumerate() {
        let numerator = i32::try_from(index).unwrap_or(i32::MAX);
        *sample = i16::try_from(i32::from(*sample) * numerator / denominator).unwrap_or(*sample);
    }
}

#[cfg(feature = "webrtc-media")]
fn synthesize_extension(last_frame: &[i16], output: &mut Vec<i16>, frame_samples: usize) {
    output.clear();
    output.resize(frame_samples, 0);
    let tail = last_frame.last().copied().unwrap_or(0);
    let fade_samples = frame_samples.min(240);
    let denominator = i32::try_from(fade_samples.max(1)).unwrap_or(i32::MAX);
    for (index, sample) in output.iter_mut().take(fade_samples).enumerate() {
        let remaining = denominator.saturating_sub(i32::try_from(index).unwrap_or(i32::MAX));
        *sample = i16::try_from(i32::from(tail) * remaining / denominator).unwrap_or(0);
    }
}

#[cfg(feature = "webrtc-media")]
fn play_packet(
    packet: &BufferedRtpPacket,
    decoder: &mut IncomingAudioDecoder,
    sink: &dyn crate::audio::AudioSink,
    jitter_buffer: &AdaptiveJitterBuffer,
) {
    let succeeded = decoder.write_packet(&packet.payload, sink);
    note_remote_playout(false, succeeded, jitter_buffer.playout_delay_ms());
}

#[cfg(feature = "webrtc-media")]
fn play_missing_packet(
    decoder: &mut IncomingAudioDecoder,
    sink: &dyn crate::audio::AudioSink,
    jitter_buffer: &mut AdaptiveJitterBuffer,
    skip_expected: bool,
) {
    let following_payload = jitter_buffer.next_payload();
    let succeeded = decoder.write_concealment(following_payload.as_deref(), sink);
    if skip_expected {
        jitter_buffer.skip_expected();
    }
    note_remote_playout(true, succeeded, jitter_buffer.playout_delay_ms());
}

#[cfg(feature = "webrtc-media")]
fn drain_expected_packets(
    decoder: &mut IncomingAudioDecoder,
    sink: &dyn crate::audio::AudioSink,
    jitter_buffer: &mut AdaptiveJitterBuffer,
    deadline: &mut Instant,
) {
    while let Some(packet) = jitter_buffer.take_expected() {
        play_packet(&packet, decoder, sink, jitter_buffer);
        *deadline += std::time::Duration::from_millis(decoder.frame_duration_ms());
    }
}

#[cfg(feature = "webrtc-media")]
struct IncomingPlayout {
    sink: Arc<dyn crate::audio::AudioSink>,
    decoder: IncomingAudioDecoder,
    jitter_buffer: AdaptiveJitterBuffer,
    arrival_jitter: ArrivalJitter,
    frame_duration: std::time::Duration,
    deadline: Option<Instant>,
    clock_rate: u32,
    /// Concealment frames played in a row without any new packet arriving.
    blind_concealments: u32,
}

#[cfg(feature = "webrtc-media")]
impl IncomingPlayout {
    fn new(
        first: &ReceivedRtpPacket,
        is_opus: bool,
        payload_type: u8,
        clock_rate: u32,
        sink: Arc<dyn crate::audio::AudioSink>,
    ) -> Self {
        let decoder = IncomingAudioDecoder::new(is_opus, payload_type, &first.packet.payload);
        let frame_duration = std::time::Duration::from_millis(decoder.frame_duration_ms());
        Self {
            sink,
            jitter_buffer: AdaptiveJitterBuffer::new(decoder.frame_duration_ms()),
            decoder,
            arrival_jitter: ArrivalJitter::default(),
            frame_duration,
            deadline: None,
            clock_rate,
            blind_concealments: 0,
        }
    }

    fn handle_timeout(&mut self, playout_deadline: Instant) {
        if self.jitter_buffer.can_expand_delay() {
            self.jitter_buffer.expand_delay();
            crate::audio::audio_debug_log(|| {
                format!(
                    "packet late: delay extension, jitter delay -> {} ms",
                    self.jitter_buffer.playout_delay_ms()
                )
            });
            self.decoder.write_delay_extension(self.sink.as_ref());
            note_delay_expansion(self.jitter_buffer.playout_delay_ms());
        } else if self.jitter_buffer.has_later_packet() {
            // A newer packet is buffered, so the expected one is really lost.
            crate::audio::audio_debug_log(|| "packet lost: concealment".to_owned());
            play_missing_packet(
                &mut self.decoder,
                self.sink.as_ref(),
                &mut self.jitter_buffer,
                true,
            );
        } else if self.blind_concealments < MAX_BLIND_CONCEALMENTS {
            // Nothing newer has arrived, so the expected packet is only late (or
            // the sender is silent). Conceal, but do NOT consume its sequence
            // number: skipping it would throw the real packet away when it shows
            // up a few milliseconds later, and leave the playout schedule with no
            // cushion, which turns a single hiccup into ~50% concealed frames.
            self.blind_concealments += 1;
            crate::audio::audio_debug_log(|| "packet late: concealment".to_owned());
            play_missing_packet(
                &mut self.decoder,
                self.sink.as_ref(),
                &mut self.jitter_buffer,
                false,
            );
        } else {
            // The stream stalled. Stop feeding concealment, rebuffer and resume
            // from the first packet that arrives.
            crate::audio::audio_debug_log(|| "stream stalled: rebuffering".to_owned());
            self.blind_concealments = 0;
            self.jitter_buffer.restart();
            self.deadline = None;
            return;
        }
        let mut next_deadline = playout_deadline + self.frame_duration;
        drain_expected_packets(
            &mut self.decoder,
            self.sink.as_ref(),
            &mut self.jitter_buffer,
            &mut next_deadline,
        );
        self.deadline = Some(next_deadline);
    }

    fn handle_packet(&mut self, received: ReceivedRtpPacket) {
        let jitter_ms = self.arrival_jitter.observe(
            received.packet.header.sequence_number,
            received.packet.header.timestamp,
            received.arrival,
            self.clock_rate,
        );
        let insertion = self.jitter_buffer.insert(
            BufferedRtpPacket {
                sequence: received.packet.header.sequence_number,
                payload: received.packet.payload,
            },
            jitter_ms,
        );
        if matches!(
            insertion,
            JitterInsert::Accepted | JitterInsert::Resynchronized
        ) {
            note_remote_arrival(jitter_ms);
            self.blind_concealments = 0;
        }
        if insertion == JitterInsert::Resynchronized {
            self.deadline = None;
        }
        if self.jitter_buffer.can_start() {
            self.start_playout();
        } else if let Some(mut next_deadline) = self.deadline {
            drain_expected_packets(
                &mut self.decoder,
                self.sink.as_ref(),
                &mut self.jitter_buffer,
                &mut next_deadline,
            );
            self.deadline = Some(next_deadline);
        }
    }

    fn start_playout(&mut self) {
        self.jitter_buffer.start();
        let mut next_deadline = Instant::now();
        for _ in 0..self.jitter_buffer.startup_frames() {
            if let Some(packet) = self.jitter_buffer.take_expected() {
                play_packet(
                    &packet,
                    &mut self.decoder,
                    self.sink.as_ref(),
                    &self.jitter_buffer,
                );
            } else {
                play_missing_packet(
                    &mut self.decoder,
                    self.sink.as_ref(),
                    &mut self.jitter_buffer,
                    true,
                );
            }
            next_deadline += self.frame_duration;
        }
        drain_expected_packets(
            &mut self.decoder,
            self.sink.as_ref(),
            &mut self.jitter_buffer,
            &mut next_deadline,
        );
        self.deadline = Some(next_deadline);
    }
}

#[cfg(feature = "webrtc-media")]
fn spawn_rtp_reader(track: Arc<dyn TrackRemote>) -> mpsc::Receiver<ReceivedRtpPacket> {
    let (packet_tx, packet_rx) = mpsc::channel(RTP_RECEIVE_QUEUE);
    tokio::spawn(async move {
        while let Some(event) = track.poll().await {
            match event {
                TrackRemoteEvent::OnRtpPacket(packet) => {
                    if matches!(
                        packet_tx.try_send(ReceivedRtpPacket {
                            packet,
                            arrival: Instant::now(),
                        }),
                        Err(mpsc::error::TrySendError::Closed(_))
                    ) {
                        break;
                    }
                }
                TrackRemoteEvent::OnEnding | TrackRemoteEvent::OnEnded => break,
                _ => {}
            }
        }
    });
    packet_rx
}

#[cfg(feature = "webrtc-media")]
async fn run_incoming_audio(
    track: Arc<dyn TrackRemote>,
    sink: Arc<dyn crate::audio::AudioSink>,
    remote_started: RemoteStarted,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut packet_rx = spawn_rtp_reader(Arc::clone(&track));
    let first = tokio::select! {
        _ = shutdown_rx.changed() => return,
        packet = packet_rx.recv() => match packet {
            Some(p) => p,
            None => return,
        },
    };
    let negotiated_codec = track
        .codec(first.packet.header.ssrc)
        .await
        .unwrap_or_default();
    let codec_name = negotiated_codec.mime_type.to_ascii_lowercase();
    let is_pcma = codec_name.contains("pcma") || codec_name.contains("g.711 a-law");
    #[cfg(feature = "opus-media")]
    let is_opus = codec_name.contains("opus");
    #[cfg(not(feature = "opus-media"))]
    let is_opus = false;
    let payload_type = if is_pcma {
        Some(8)
    } else if codec_name.contains("pcmu") || codec_name.contains("g.711") {
        Some(0)
    } else {
        None
    };
    if let Some(callback) = remote_started
        .lock()
        .ok()
        .and_then(|handler| handler.as_ref().cloned())
    {
        callback();
    }
    let first_payload_type = first.packet.header.payload_type;
    let mut playout = IncomingPlayout::new(
        &first,
        is_opus,
        if is_opus {
            first_payload_type
        } else {
            payload_type.unwrap_or(first_payload_type)
        },
        negotiated_codec.clock_rate.max(1),
        sink,
    );
    let mut pending = Some(first);
    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let received = if let Some(packet) = pending.take() {
            Some(packet)
        } else if let Some(playout_deadline) = playout.deadline {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                packet = packet_rx.recv() => packet,
                () = tokio::time::sleep_until(playout_deadline.into()) => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    playout.handle_timeout(playout_deadline);
                    continue;
                }
            }
        } else {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                packet = packet_rx.recv() => packet,
            }
        };
        let Some(received) = received else {
            break;
        };
        playout.handle_packet(received);
    }
    crate::audio::audio_debug_log(|| "incoming audio loop terminated cleanly".to_owned());
}

#[cfg(all(test, feature = "webrtc-media"))]
mod tests {
    use super::{
        AdaptiveJitterBuffer, AudioCodec, AudioSignalQuality, AudioSignalState, BufferedRtpPacket,
        IncomingAudioDecoder, IncomingPlayout, JitterInsert, PcmuFrameAssembler, ReceivedRtpPacket,
        rtc_ice_server,
    };
    #[cfg(feature = "opus-media")]
    use super::{
        OPUS_FRAME_SAMPLES, OPUS_MAX_FRAME_SAMPLES, OpusTransmitProfile, configured_opus_encoder,
        encode_opus_frame,
    };
    use bytes::Bytes;
    use std::{sync::Arc, time::Instant};

    #[cfg(feature = "cpal-audio")]
    #[tokio::test(flavor = "current_thread")]
    async fn microphone_worker_encodes_captured_audio_and_shuts_down() {
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(2);
        let state = super::MicrophoneEncoderState {
            codec: AudioCodec::G711(0),
            processor: crate::audio::MicrophoneProcessor::new(8_000),
            muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_recorder: None,
            resampler: crate::audio::LinearResampler::new(),
            resample_buffer: Vec::with_capacity(160),
            pcmu_buffer: PcmuFrameAssembler::default(),
            g711_buffer: Vec::with_capacity(160),
            #[cfg(feature = "opus-media")]
            opus_buffer: Vec::new(),
            #[cfg(feature = "opus-media")]
            opus_encoder: None,
        };
        let worker = super::MicrophoneEncodingWorker::spawn(state, audio_tx).unwrap();
        worker.enqueue(&[1_000; 160], 8_000);

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), audio_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let super::OutgoingAudioFrame::G711 {
            payload_type,
            payload,
        } = frame
        else {
            panic!("G.711 encoder worker returned the wrong codec");
        };
        assert_eq!(payload_type, 0);
        assert_eq!(payload.len(), 160);
        drop(worker);
    }

    #[cfg(feature = "cpal-audio")]
    #[tokio::test(flavor = "current_thread")]
    async fn microphone_worker_allocates_when_recycled_pool_is_empty() {
        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel(10);
        let state = super::MicrophoneEncoderState {
            codec: AudioCodec::G711(0),
            processor: crate::audio::MicrophoneProcessor::new(8_000),
            muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_recorder: None,
            resampler: crate::audio::LinearResampler::new(),
            resample_buffer: Vec::with_capacity(160),
            pcmu_buffer: PcmuFrameAssembler::default(),
            g711_buffer: Vec::with_capacity(160),
            #[cfg(feature = "opus-media")]
            opus_buffer: Vec::new(),
            #[cfg(feature = "opus-media")]
            opus_encoder: None,
        };
        let worker = super::MicrophoneEncodingWorker::spawn(state, audio_tx).unwrap();
        // Drain recycled buffer queue to ensure buffer pool is completely empty
        if let Ok(recycled) = worker.recycled.try_lock() {
            while recycled.try_recv().is_ok() {}
        }
        worker.enqueue(&[1_000; 160], 8_000);

        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), audio_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let super::OutgoingAudioFrame::G711 {
            payload_type,
            payload,
        } = frame
        else {
            panic!("G.711 encoder worker returned the wrong codec");
        };
        assert_eq!(payload_type, 0);
        assert_eq!(payload.len(), 160);
        drop(worker);
    }

    #[test]
    fn ice_server_credentials_are_preserved() {
        let mapped = rtc_ice_server(&crate::voice::IceServer {
            urls: vec!["turn:relay.example:3478".into()],
            username: Some("user".into()),
            credential: Some("secret".into()),
        });
        assert_eq!(mapped.username, "user");
        assert_eq!(mapped.credential, "secret");

        let stun = rtc_ice_server(&crate::voice::IceServer {
            urls: vec!["stun:stun.example:3478".into()],
            username: None,
            credential: None,
        });
        assert!(stun.username.is_empty());
        assert!(stun.credential.is_empty());
    }

    fn jitter_packet(sequence: u16) -> BufferedRtpPacket {
        BufferedRtpPacket {
            sequence,
            payload: Bytes::copy_from_slice(&sequence.to_le_bytes()),
        }
    }

    fn received_g711_packet(sequence: u16, sample: i16) -> ReceivedRtpPacket {
        ReceivedRtpPacket {
            packet: rtc::rtp::Packet {
                header: rtc::rtp::Header {
                    version: 2,
                    payload_type: 0,
                    sequence_number: sequence,
                    timestamp: u32::from(sequence).saturating_mul(160),
                    ..Default::default()
                },
                payload: Bytes::from(crate::voice::g711_encode(0, &[sample; 160])),
            },
            arrival: Instant::now(),
        }
    }

    #[test]
    fn jitter_buffer_reorders_packets_before_playout() {
        let mut buffer = AdaptiveJitterBuffer::new(20);
        assert_eq!(
            buffer.insert(jitter_packet(12), 0.0),
            JitterInsert::Accepted
        );
        assert_eq!(
            buffer.insert(jitter_packet(10), 0.0),
            JitterInsert::Accepted
        );
        assert_eq!(
            buffer.insert(jitter_packet(11), 0.0),
            JitterInsert::Accepted
        );
        assert!(buffer.can_start());
        buffer.start();

        assert_eq!(buffer.take_expected().unwrap().sequence, 10);
        assert_eq!(buffer.take_expected().unwrap().sequence, 11);
        assert_eq!(buffer.take_expected().unwrap().sequence, 12);
    }

    #[test]
    fn jitter_buffer_handles_sequence_wrap_duplicates_and_late_packets() {
        let mut buffer = AdaptiveJitterBuffer::new(20);
        assert_eq!(
            buffer.insert(jitter_packet(u16::MAX), 0.0),
            JitterInsert::Accepted
        );
        assert_eq!(buffer.insert(jitter_packet(0), 0.0), JitterInsert::Accepted);
        assert_eq!(buffer.insert(jitter_packet(1), 0.0), JitterInsert::Accepted);
        assert_eq!(
            buffer.insert(jitter_packet(1), 0.0),
            JitterInsert::Duplicate
        );
        buffer.start();
        assert_eq!(buffer.take_expected().unwrap().sequence, u16::MAX);
        assert_eq!(buffer.take_expected().unwrap().sequence, 0);
        assert_eq!(
            buffer.insert(jitter_packet(u16::MAX), 0.0),
            JitterInsert::Late
        );
    }

    #[test]
    fn jitter_buffer_increases_delay_when_arrival_jitter_rises() {
        let mut buffer = AdaptiveJitterBuffer::new(20);
        assert_eq!(buffer.playout_delay_ms(), 60);
        buffer.insert(jitter_packet(1), 40.0);
        assert!(buffer.target_packets > buffer.playout_packets);
        while buffer.can_expand_delay() {
            buffer.expand_delay();
        }
        assert_eq!(buffer.playout_delay_ms(), 160);
    }

    #[test]
    fn incoming_playout_reorders_g711_and_conceals_a_deadline_miss() {
        let arrival = Instant::now();
        let mut first = received_g711_packet(10, 1_000);
        first.arrival = arrival;
        let mut third = received_g711_packet(12, 3_000);
        third.arrival = arrival + std::time::Duration::from_millis(40);
        let mut second = received_g711_packet(11, 2_000);
        second.arrival = arrival + std::time::Duration::from_millis(45);
        let sink = Arc::new(crate::audio::QueuedAudioSink::new());
        let mut playout = IncomingPlayout::new(&first, false, 0, 8_000, sink.clone());
        playout.handle_packet(first);
        playout.handle_packet(third);
        assert_eq!(sink.queued_samples(), 0);
        playout.handle_packet(second);
        assert_eq!(sink.queued_samples(), 480);

        let deadline = playout.deadline.unwrap();
        playout.handle_timeout(deadline);
        assert_eq!(sink.queued_samples(), 640);
    }

    #[test]
    fn stalled_stream_rebuffers_instead_of_dropping_real_packets() {
        let arrival = Instant::now();
        let step = std::time::Duration::from_millis(20);
        let sink = Arc::new(crate::audio::QueuedAudioSink::new());
        let first = received_g711_packet(10, 1_000);
        let mut playout = IncomingPlayout::new(&first, false, 0, 8_000, sink.clone());
        for sequence in 10..13u16 {
            let mut packet = received_g711_packet(sequence, 1_000);
            packet.arrival = arrival + step * u32::from(sequence - 10);
            playout.handle_packet(packet);
        }
        assert!(playout.deadline.is_some());

        // The sender goes quiet: playout must give up concealing and rebuffer.
        for _ in 0..40 {
            if let Some(deadline) = playout.deadline {
                playout.handle_timeout(deadline);
            }
        }
        assert!(playout.deadline.is_none());

        // When packets resume they are played, not discarded as "late".
        let before = sink.queued_samples();
        for sequence in 13..33u16 {
            let mut packet = received_g711_packet(sequence, 1_000);
            packet.arrival = arrival + step * u32::from(sequence - 10);
            playout.handle_packet(packet);
        }
        assert!(sink.queued_samples() >= before + 3 * 160);
    }

    #[test]
    fn signal_quality_classifies_measured_audio_health() {
        let quality =
            |packets, loss_percent, jitter_ms, decode_error_percent, age| AudioSignalQuality {
                packets,
                loss_percent,
                jitter_ms,
                decode_error_percent,
                last_packet_age_ms: age,
                playout_delay_ms: 60,
                playout_underruns: 0,
                concealed_frames: 0,
                speaker_underruns: 0,
            };

        assert_eq!(
            quality(0, 0.0, 0.0, 0.0, None).state(),
            AudioSignalState::Waiting
        );
        assert_eq!(
            quality(20, 0.5, 8.0, 0.0, Some(20)).state(),
            AudioSignalState::Good
        );
        assert_eq!(
            quality(20, 3.0, 20.0, 0.0, Some(20)).state(),
            AudioSignalState::Fair
        );
        assert_eq!(
            quality(20, 9.0, 20.0, 0.0, Some(20)).state(),
            AudioSignalState::Poor
        );
        assert_eq!(
            quality(20, 0.0, 0.0, 0.0, Some(2_000)).state(),
            AudioSignalState::Interrupted
        );
    }

    #[cfg(feature = "opus-media")]
    #[test]
    fn opus_transmit_profile_spends_bits_and_fec_for_current_path_health() {
        let quality = |loss_percent, jitter_ms| AudioSignalQuality {
            packets: 20,
            loss_percent,
            jitter_ms,
            decode_error_percent: 0.0,
            last_packet_age_ms: Some(20),
            playout_delay_ms: 60,
            playout_underruns: 0,
            concealed_frames: 0,
            speaker_underruns: 0,
        };
        let good = OpusTransmitProfile::from_quality(quality(0.5, 8.0));
        assert_eq!(good.bitrate_bps, 56_000);
        assert!(good.fec);
        assert_eq!(good.packet_loss_percent, 5);
        assert_eq!(good.frame_samples, OPUS_FRAME_SAMPLES);

        let poor = OpusTransmitProfile::from_quality(quality(9.0, 90.0));
        assert_eq!(poor.bitrate_bps, 36_000);
        assert!(poor.fec);
        assert_eq!(poor.packet_loss_percent, 20);
        assert_eq!(poor.frame_samples, OPUS_FRAME_SAMPLES);
    }

    #[cfg(feature = "opus-media")]
    #[test]
    fn opus_encoder_accepts_adaptive_forty_millisecond_frames() {
        let mut encoder = configured_opus_encoder().unwrap();
        let profile = OpusTransmitProfile {
            bitrate_bps: 28_000,
            packet_loss_percent: 18,
            frame_samples: OPUS_MAX_FRAME_SAMPLES,
            fec: true,
        };
        profile.apply(&mut encoder);
        let packet = encode_opus_frame(&mut encoder, &[0; OPUS_MAX_FRAME_SAMPLES]).unwrap();
        let mut decoder = opus_pure::OpusDecoder::new(48_000, 1).unwrap();
        let mut output = vec![0; OPUS_MAX_FRAME_SAMPLES];
        assert_eq!(
            decoder
                .decode_s16(&packet, OPUS_MAX_FRAME_SAMPLES, &mut output)
                .unwrap(),
            OPUS_MAX_FRAME_SAMPLES
        );
    }

    #[test]
    fn codec_names_select_the_expected_rtp_mode() {
        assert_eq!(AudioCodec::from_name(Some("pcmu")), AudioCodec::G711(0));
        assert_eq!(AudioCodec::from_name(Some("PCMA")), AudioCodec::G711(8));
        #[cfg(feature = "opus-media")]
        assert_eq!(AudioCodec::from_name(Some("opus/48000")), AudioCodec::Opus);
        assert_eq!(
            AudioCodec::from_name(Some("unknown")),
            AudioCodec::from_name(None)
        );
    }

    #[test]
    fn pcmu_frames_are_buffered_until_a_full_twenty_ms_payload_exists() {
        let mut assembler = PcmuFrameAssembler::default();
        assert!(assembler.push(&[1; 159]).is_empty());
        let frames = assembler.push(&[2]);
        assert_eq!(
            frames,
            vec![1; 159].into_iter().chain([2]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pcmu_frames_preserve_order_across_multiple_callbacks() {
        let mut assembler = PcmuFrameAssembler::default();
        let mut payload = vec![0; 80];
        payload.extend([1; 240]);
        let frames = assembler.push(&payload);
        assert_eq!(frames.len(), 320);
        assert_eq!(
            frames[..160],
            vec![0; 80].into_iter().chain([1; 80]).collect::<Vec<_>>()
        );
        assert_eq!(frames[160..], vec![1; 160]);
        assert!(assembler.push(&[2; 80]).is_empty());
    }

    #[cfg(feature = "opus-media")]
    #[test]
    fn opus_codec_round_trips_a_twenty_millisecond_mono_frame() {
        let mut encoder = configured_opus_encoder().unwrap();
        assert_eq!(encoder.bitrate_bps, 48_000);
        assert_eq!(encoder.complexity, 10);
        assert_eq!(encoder.rate_control, opus_pure::RateControl::ConstrainedVbr);
        assert!(encoder.use_inband_fec);
        assert_eq!(encoder.packet_loss_perc, 5);
        let input: Vec<i16> = (0..OPUS_FRAME_SAMPLES)
            .map(|index| ((index as f32 * 0.05).sin() * 12_000.0) as i16)
            .collect();
        let opus_packet = encode_opus_frame(&mut encoder, &input).unwrap();
        assert!(!opus_packet.is_empty());
        let mut decoder = opus_pure::OpusDecoder::new(48_000, 1).unwrap();
        let mut output = vec![0i16; OPUS_FRAME_SAMPLES];
        let sample_count = decoder
            .decode_s16(&opus_packet, OPUS_FRAME_SAMPLES, &mut output)
            .unwrap();
        assert_eq!(sample_count, OPUS_FRAME_SAMPLES);
        assert!(output.iter().any(|sample| *sample != 0));
    }

    #[cfg(feature = "opus-media")]
    #[test]
    fn incoming_opus_decoder_conceals_a_missing_frame_without_shortening_playout() {
        let mut encoder = configured_opus_encoder().unwrap();
        let input = (0..OPUS_FRAME_SAMPLES)
            .map(|index| if index % 48 < 24 { 8_000 } else { -8_000 })
            .collect::<Vec<_>>();
        let first = encode_opus_frame(&mut encoder, &input).unwrap();
        let _lost = encode_opus_frame(&mut encoder, &input).unwrap();
        let following = encode_opus_frame(&mut encoder, &input).unwrap();
        let sink = crate::audio::QueuedAudioSink::new();
        let mut decoder = IncomingAudioDecoder::new(true, 111, &first);

        assert!(decoder.write_packet(&first, &sink));
        assert!(decoder.write_concealment(Some(&following), &sink));
        assert!(decoder.write_packet(&following, &sink));
        assert_eq!(sink.queued_samples(), OPUS_FRAME_SAMPLES * 3);
    }

    #[tokio::test]
    async fn create_offer_returns_valid_sdp_without_blocking() {
        let ice_servers = vec![crate::voice::IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        }];
        let peer = super::WebRtcPeer::new(&ice_servers, None, None, None)
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let offer = peer.create_offer().await.unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed < std::time::Duration::from_millis(500));
        assert!(offer.contains("m=audio"));
        assert!(offer.contains("a=mid:"));
    }
}
