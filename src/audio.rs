#[cfg(feature = "cpal-audio")]
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub const VOICE_SAMPLE_RATE: u32 = 8_000;
static SPEAKER_HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static MICROPHONE_LEVEL_BITS: AtomicU32 = AtomicU32::new(0);
static SPEAKER_TARGET_BUFFER_MS: AtomicU32 = AtomicU32::new(80);
static SPEAKER_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
static SPEAKER_PLAYOUT_STARTED: AtomicBool = AtomicBool::new(false);
static SPEAKER_MUTED: AtomicBool = AtomicBool::new(false);

/// Enabled with the `NEKTOME_AUDIO_DEBUG` environment variable. Diagnostics only.
pub(crate) fn audio_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("NEKTOME_AUDIO_DEBUG").is_some())
}

pub(crate) fn audio_debug_log(message: impl FnOnce() -> String) {
    if audio_debug() {
        eprintln!("[audio {}ms] {}", now_ms() % 100_000, message());
    }
}

pub fn set_speaker_muted(muted: bool) {
    SPEAKER_MUTED.store(muted, Ordering::Relaxed);
}

#[must_use]
pub fn speaker_muted() -> bool {
    SPEAKER_MUTED.load(Ordering::Relaxed)
}

/// Updates the target depth used to compensate for network and device clock drift.
/// The minimum is clamped to 80 ms so that the native audio pre-buffer always
/// has a healthy cushion regardless of what the jitter-buffer reports.
pub fn set_speaker_playout_delay(delay_ms: u64) {
    SPEAKER_TARGET_BUFFER_MS.store(
        u32::try_from(delay_ms.clamp(80, 500)).unwrap_or(500),
        Ordering::Relaxed,
    );
}

/// Returns the number of times an active native speaker queue ran dry.
#[must_use]
pub fn speaker_underruns() -> u64 {
    SPEAKER_UNDERRUNS.load(Ordering::Relaxed)
}

/// Clears native speaker queue metrics for a newly connected peer.
pub fn reset_speaker_playout_metrics() {
    SPEAKER_UNDERRUNS.store(0, Ordering::Relaxed);
    SPEAKER_PLAYOUT_STARTED.store(false, Ordering::Relaxed);
    SPEAKER_TARGET_BUFFER_MS.store(80, Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn note_speaker_activity(pcm: &[i16]) {
    let peak = pcm
        .iter()
        .map(|sample| (f32::from(*sample) / 32768.0).abs())
        .fold(0.0, f32::max);
    if peak > 0.1 {
        SPEAKER_HOLD_UNTIL_MS.store(now_ms().saturating_add(250), Ordering::Relaxed);
    }
}

fn speaker_is_active() -> bool {
    now_ms() < SPEAKER_HOLD_UNTIL_MS.load(Ordering::Relaxed)
}

pub fn record_microphone_level(pcm: &[i16]) {
    if pcm.is_empty() {
        MICROPHONE_LEVEL_BITS.store(0.0f32.to_bits(), Ordering::Relaxed);
        return;
    }
    let sum = pcm
        .iter()
        .map(|sample| {
            let normalized = f32::from(*sample) / 32768.0;
            normalized * normalized
        })
        .sum::<f32>();
    let rms = (sum / pcm.len() as f32).sqrt().clamp(0.0, 1.0);
    MICROPHONE_LEVEL_BITS.store(rms.to_bits(), Ordering::Relaxed);
}

pub fn microphone_level() -> f32 {
    f32::from_bits(MICROPHONE_LEVEL_BITS.load(Ordering::Relaxed)).clamp(0.0, 1.0)
}

/// Stateful microphone dynamics matching the legacy client's gate/compressor
/// defaults. Samples are normalized to [-1, 1] internally and clipped safely.
pub struct AudioDynamicsProcessor {
    envelope: f32,
    attack: f32,
    release: f32,
    sample_rate: u32,
    pub noise_gate_threshold: f32,
    pub compressor_threshold: f32,
    pub compressor_ratio: f32,
    pub makeup_gain: f32,
    pub enabled: bool,
}

impl AudioDynamicsProcessor {
    pub fn new(sample_rate: u32) -> Self {
        let mut processor = Self {
            envelope: 0.0,
            attack: 0.0,
            release: 0.0,
            sample_rate: sample_rate.max(1),
            noise_gate_threshold: 0.002,
            compressor_threshold: 0.12,
            compressor_ratio: 2.5,
            makeup_gain: 1.35,
            enabled: true,
        };
        processor.set_sample_rate(sample_rate);
        processor
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate.max(1);
        let rate = self.sample_rate as f32;
        // 12 ms attack preserves vocal consonant transients for crisp diction,
        // while 160 ms release prevents pumping artifacts between syllables.
        self.attack = (-1.0 / (0.012 * rate)).exp();
        self.release = (-1.0 / (0.160 * rate)).exp();
    }

    pub fn process(&mut self, pcm: &mut [i16]) {
        if !self.enabled {
            return;
        }
        for sample in pcm {
            let normalized = f32::from(*sample) / 32768.0;
            let amplitude = normalized.abs();
            let coefficient = if amplitude > self.envelope {
                self.attack
            } else {
                self.release
            };
            self.envelope = coefficient * self.envelope + (1.0 - coefficient) * amplitude;

            let mut gain = 1.0f32;
            if self.noise_gate_threshold > 0.0 && self.envelope < self.noise_gate_threshold {
                let gate = (self.envelope / self.noise_gate_threshold).clamp(0.0, 1.0);
                gain *= gate * gate;
            }
            if self.compressor_ratio > 1.0 && self.compressor_threshold > 0.0 {
                let knee = (self.compressor_threshold * 0.3).min(0.08);
                let lower = (self.compressor_threshold - knee).max(0.0);
                let upper = self.compressor_threshold + knee;
                if self.envelope > upper {
                    let compressed = self.compressor_threshold
                        + (self.envelope - self.compressor_threshold) / self.compressor_ratio;
                    gain = gain.min(compressed / self.envelope);
                } else if self.envelope > lower && knee > 0.0 {
                    let t = (self.envelope - lower) / (upper - lower);
                    let target_comp = self.compressor_threshold
                        + (self.envelope - self.compressor_threshold) / self.compressor_ratio;
                    let target_gain = target_comp / self.envelope;
                    let blend = t * t * (3.0 - 2.0 * t);
                    let knee_gain = 1.0 + (target_gain - 1.0) * blend;
                    gain = gain.min(knee_gain);
                }
            }
            let output = (normalized * gain * self.makeup_gain).clamp(-0.999, 0.999);
            *sample = (output * 32768.0) as i16;
        }
    }
}

/// Parameters defining a comprehensive vocal character profile combining tone shaping,
/// saturation, and compression dynamics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterPresetParams {
    pub presence_db: f32,
    pub drive: f32,
    pub asymmetry: f32,
    pub bias: f32,
    pub proximity_db: f32,
    pub air_db: f32,
    pub compressor_ratio: f32,
    pub makeup_gain: f32,
}

/// Presets for [`VocalWarmthProcessor`] optimized for speech intelligibility,
/// natural dialogue, and analog character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SaturationProfile {
    /// Natural conversational voice with pure clarity and minimal coloration.
    #[default]
    NaturalVoice,
    /// Clean studio dialogue with subtle body and near-mic presence.
    StudioDialogue,
    /// Warm, intimate tone for long-form podcasts and comfortable listening.
    PodcastWarm,
    /// Articulate radio voice with punchy mids and crisp diction.
    FmVoice,
    /// Deep, cinematic voice with expansive warmth and rich lower body.
    CinematicVoice,
    /// Maximum intelligibility with transparent transient articulation.
    VoiceClarity,
    /// Tight newsreader delivery with authoritative attack and presence.
    NewsBroadcast,
    /// Soft, cozy, low-fatigue voice for narration, audiobooks, and storytelling.
    SoftStoryteller,
    /// Dynamic, commanding tone with pronounced harmonic drive.
    ConfidentSpeaker,
    /// Vintage broadcast tone with intentional tube/carbon-mic grit.
    VintageRadio,
    /// Confident, calm, authoritative voice without harshness.
    Authority,
    /// Dense, deep-seated, assertive tone with low-end gravity.
    Dominant,
    /// Bold attack, intense presence, and commanding delivery.
    Commanding,
    /// Close, warm, but firmly controlling and persuasive tone.
    IntimateAuthority,
    /// Hard, aggressively expressive, commanding tone.
    HardAuthority,
    /// Dark, velvety, deeply saturated vocal weight.
    DarkVelvet,
    /// High-impact broadcast command with maximum articulation.
    CommandRadio,
    /// High-presence, commanding leader tone with heavy authority.
    DominantAuthority,
    /// Soft, warm, very intimate persuasive tone.
    #[serde(alias = "VelvetCharmer")]
    VelvetPersuasion,
    /// Dark, rich, confident charismatic voice.
    DarkCharisma,
    /// Neutral surface tone with subtle mysterious coloration.
    MaskedIntent,
    /// Quiet, dense, intimate authority.
    WhisperAuthority,
    /// Playful, confident, lightly provocative delivery.
    PlayfulControl,
    /// Cold, collected, emotionally controlled charisma.
    ColdCharisma,
    /// Expressive vintage antihero and villain timbre.
    ElegantVillain,
    /// Smooth, soft, enveloping hypnotic vocal warmth.
    HypnoticWarmth,
    /// Class-A triode warmth with dominant 2nd harmonic and soft peak compression.
    GoldenVintage,
    /// Analog reel tape saturation with subtle glue and gentle harmonic blend.
    WarmTape,
    /// High-drive broadcast coloration reminiscent of pushed 12AX7 studio preamps.
    #[serde(alias = "HotTube")]
    HotTube,
    /// Ultra-transparent air and presence with minimal harmonic distortion.
    #[serde(alias = "SubtleAir")]
    SubtleAir,
    /// Thick vintage overdrive and lo-fi analog crunch.
    #[serde(alias = "HeavyDrive")]
    HeavyDrive,
    /// Custom user-tuned drive, asymmetry, and bias settings.
    Custom,
}

impl SaturationProfile {
    /// Dialogue and speech clarity profiles recommended for everyday conversation.
    pub const DIALOGUE: [Self; 10] = [
        Self::NaturalVoice,
        Self::StudioDialogue,
        Self::PodcastWarm,
        Self::FmVoice,
        Self::CinematicVoice,
        Self::VoiceClarity,
        Self::NewsBroadcast,
        Self::SoftStoryteller,
        Self::ConfidentSpeaker,
        Self::VintageRadio,
    ];

    /// Assertive, commanding, and authoritative vocal profiles.
    pub const DOMINANT: [Self; 8] = [
        Self::Authority,
        Self::Dominant,
        Self::Commanding,
        Self::IntimateAuthority,
        Self::HardAuthority,
        Self::DarkVelvet,
        Self::CommandRadio,
        Self::DominantAuthority,
    ];

    /// Dramatic, character, and psychological vocal tone profiles.
    pub const CHARACTER: [Self; 8] = [
        Self::VelvetPersuasion,
        Self::DarkCharisma,
        Self::MaskedIntent,
        Self::WhisperAuthority,
        Self::PlayfulControl,
        Self::ColdCharisma,
        Self::ElegantVillain,
        Self::HypnoticWarmth,
    ];

    /// Analog vintage console, tape, and vacuum tube hardware emulation profiles.
    pub const ANALOG: [Self; 5] = [
        Self::GoldenVintage,
        Self::WarmTape,
        Self::HotTube,
        Self::SubtleAir,
        Self::HeavyDrive,
    ];

    /// All available saturation profile choices for UI presentation.
    pub const ALL: [Self; 32] = [
        Self::NaturalVoice,
        Self::StudioDialogue,
        Self::PodcastWarm,
        Self::FmVoice,
        Self::CinematicVoice,
        Self::VoiceClarity,
        Self::NewsBroadcast,
        Self::SoftStoryteller,
        Self::ConfidentSpeaker,
        Self::VintageRadio,
        Self::Authority,
        Self::Dominant,
        Self::Commanding,
        Self::IntimateAuthority,
        Self::HardAuthority,
        Self::DarkVelvet,
        Self::CommandRadio,
        Self::DominantAuthority,
        Self::VelvetPersuasion,
        Self::DarkCharisma,
        Self::MaskedIntent,
        Self::WhisperAuthority,
        Self::PlayfulControl,
        Self::ColdCharisma,
        Self::ElegantVillain,
        Self::HypnoticWarmth,
        Self::GoldenVintage,
        Self::WarmTape,
        Self::HotTube,
        Self::SubtleAir,
        Self::HeavyDrive,
        Self::Custom,
    ];

    /// Short human-readable display title with icon.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NaturalVoice => "🗣️ Natural Voice (Default)",
            Self::StudioDialogue => "🎧 Studio Dialogue",
            Self::PodcastWarm => "🎙️ Podcast Warm",
            Self::FmVoice => "📻 FM Voice",
            Self::CinematicVoice => "🎬 Cinematic Voice",
            Self::VoiceClarity => "📞 Voice Clarity",
            Self::NewsBroadcast => "📰 News Broadcast",
            Self::SoftStoryteller => "🧸 Soft Storyteller",
            Self::ConfidentSpeaker => "🔥 Confident Speaker",
            Self::VintageRadio => "🕰️ Vintage Radio",
            Self::Authority => "👑 Authority",
            Self::Dominant => "🖤 Dominant",
            Self::Commanding => "⚡ Commanding",
            Self::IntimateAuthority => "🎙️ Intimate Authority",
            Self::HardAuthority => "🔥 Hard Authority",
            Self::DarkVelvet => "🥃 Dark Velvet",
            Self::CommandRadio => "📻 Command Radio",
            Self::DominantAuthority => "👑 Dominant Authority",
            Self::VelvetPersuasion => "🐍 Velvet Persuasion",
            Self::DarkCharisma => "🌑 Dark Charisma",
            Self::MaskedIntent => "🎭 Masked Intent",
            Self::WhisperAuthority => "🕯️ Whisper Authority",
            Self::PlayfulControl => "🐈 Playful Control",
            Self::ColdCharisma => "🖤 Cold Charisma",
            Self::ElegantVillain => "🎩 Elegant Villain",
            Self::HypnoticWarmth => "🔮 Hypnotic Warmth",
            Self::GoldenVintage => "🌟 Golden Vintage (Class-A)",
            Self::WarmTape => "📼 Warm Tape (Analog Reel)",
            Self::HotTube => "🎙️ Hot Tube (12AX7 Driven)",
            Self::SubtleAir => "🍃 Subtle Air (Clean Presence)",
            Self::HeavyDrive => "🎸 Heavy Drive (Lo-Fi Crunch)",
            Self::Custom => "⚙️ Custom",
        }
    }

    /// Sonic description explaining the character of this profile.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::NaturalVoice => {
                "Максимально естественная речь, прозрачность и чистый нейтральный баланс"
            }
            Self::StudioDialogue => "Чистый студийный голос, немного плотнее и ближе к микрофону",
            Self::PodcastWarm => "Тёплый близкий тембр, комфортный для продолжительного разговора",
            Self::FmVoice => "Чёткая дикция, собранная плотная середина, радийная подача",
            Self::CinematicVoice => "Глубокий, выразительный и объёмный кинематографичный голос",
            Self::VoiceClarity => "Максимум разборчивости и артикуляции при минимуме окраса",
            Self::NewsBroadcast => {
                "Собранный дикторский тембр, уверенная атака и эффект присутствия"
            }
            Self::SoftStoryteller => {
                "Мягкий, спокойный, интимный голос для повествования и историй"
            }
            Self::ConfidentSpeaker => "Энергичный, напористый и убедительный лидерский тембр",
            Self::VintageRadio => "Яркая ретро-окраска с выраженным ламповым характером",
            Self::Authority => "Уверенный, спокойный, властный тембр без излишней резкости",
            Self::Dominant => "Плотный, низко воспринимаемый, напористый голос с весом",
            Self::Commanding => "Яркая атака, сильное присутствие, командный тон",
            Self::IntimateAuthority => "Близкий, тёплый, контролирующий и убедительный",
            Self::HardAuthority => "Жёсткий, агрессивно-выразительный и напористый тембр",
            Self::DarkVelvet => "Тёмный, насыщенный, бархатный глубокий голос",
            Self::CommandRadio => "Дикторский, очень уверенный и разборчивый радийный тон",
            Self::DominantAuthority => {
                "Сильное присутствие, командный тембр, вес и максимальная лидерская уверенность"
            }
            Self::VelvetPersuasion => "Мягкий, тёплый, очень близкий и доверительный голос",
            Self::DarkCharisma => "Тёмный, насыщенный, глубокий и уверенный тембр",
            Self::MaskedIntent => "Нейтральная поверхность с лёгкой скрытой окраской",
            Self::WhisperAuthority => "Тихий, плотный, интимный и контролирующий голос",
            Self::PlayfulControl => "Игривый, самоуверенный, слегка провокационный тембр",
            Self::ColdCharisma => "Холодный, собранный, эмоционально сдержанный голос",
            Self::ElegantVillain => "Выразительный винтажный антигеройский бархатный тембр",
            Self::HypnoticWarmth => "Плавный, мягкий, обволакивающий гипнотический звук",
            Self::GoldenVintage => "Классический триодный каскад класса А с богатой 2-й гармоникой",
            Self::WarmTape => "Ленточная аналоговая компрессия и музыкальный клей",
            Self::HotTube => "Пробивной радийный/подкастерский тон с ярким гармоническим телом",
            Self::SubtleAir => "Прозрачный воздух и эффект присутствия без слышимого искажения",
            Self::HeavyDrive => "Жирный аналоговый овердрайв, винтажный кранч и лоу-фай окрас",
            Self::Custom => "Пользовательские параметры Drive, Asymmetry и Bias",
        }
    }

    /// Maps this saturation profile to its corresponding holistic character profile.
    #[must_use]
    pub const fn to_character_profile(self) -> VoiceCharacterProfile {
        match self {
            Self::NaturalVoice => VoiceCharacterProfile::Natural,
            Self::StudioDialogue => VoiceCharacterProfile::StudioDialogue,
            Self::PodcastWarm => VoiceCharacterProfile::PodcastWarm,
            Self::FmVoice => VoiceCharacterProfile::FmVoice,
            Self::CinematicVoice => VoiceCharacterProfile::CinematicVoice,
            Self::VoiceClarity => VoiceCharacterProfile::VoiceClarity,
            Self::NewsBroadcast => VoiceCharacterProfile::NewsBroadcast,
            Self::SoftStoryteller => VoiceCharacterProfile::SoftStoryteller,
            Self::ConfidentSpeaker => VoiceCharacterProfile::ConfidentSpeaker,
            Self::VintageRadio => VoiceCharacterProfile::VintageRadio,
            Self::Authority => VoiceCharacterProfile::Authority,
            Self::Dominant => VoiceCharacterProfile::Dominant,
            Self::Commanding => VoiceCharacterProfile::Commanding,
            Self::IntimateAuthority => VoiceCharacterProfile::IntimateAuthority,
            Self::HardAuthority => VoiceCharacterProfile::HardAuthority,
            Self::DarkVelvet => VoiceCharacterProfile::DarkVelvet,
            Self::CommandRadio => VoiceCharacterProfile::CommandRadio,
            Self::DominantAuthority => VoiceCharacterProfile::DominantAuthority,
            Self::VelvetPersuasion => VoiceCharacterProfile::VelvetCharmer,
            Self::DarkCharisma => VoiceCharacterProfile::DarkCharisma,
            Self::MaskedIntent => VoiceCharacterProfile::MaskedIntent,
            Self::WhisperAuthority => VoiceCharacterProfile::WhisperAuthority,
            Self::PlayfulControl => VoiceCharacterProfile::PlayfulControl,
            Self::ColdCharisma => VoiceCharacterProfile::ColdCharisma,
            Self::ElegantVillain => VoiceCharacterProfile::ElegantVillain,
            Self::HypnoticWarmth => VoiceCharacterProfile::HypnoticWarmth,
            Self::GoldenVintage => VoiceCharacterProfile::GoldenVintage,
            Self::WarmTape => VoiceCharacterProfile::WarmTape,
            Self::HotTube => VoiceCharacterProfile::HotTube,
            Self::SubtleAir => VoiceCharacterProfile::SubtleAir,
            Self::HeavyDrive => VoiceCharacterProfile::HeavyDrive,
            Self::Custom => VoiceCharacterProfile::Custom,
        }
    }

    /// Full acoustic parameters across the entire signal chain:
    /// EQ (presence, proximity, air), saturation (drive, asymmetry, bias), and dynamics.
    #[must_use]
    pub const fn full_params(self) -> Option<CharacterPresetParams> {
        self.to_character_profile().params()
    }

    /// Standard `(drive, asymmetry, bias)` tuple for preset profiles, or `None` if custom.
    #[must_use]
    pub const fn params(self) -> Option<(f32, f32, f32)> {
        match self.full_params() {
            Some(p) => Some((p.drive, p.asymmetry, p.bias)),
            None => None,
        }
    }

    /// Matches raw parameters to the closest known profile or returns [`SaturationProfile::Custom`].
    #[must_use]
    pub fn from_params(drive: f32, asymmetry: f32, bias: f32) -> Self {
        for profile in Self::ALL {
            if let Some((d, a, b)) = profile.params() {
                if (drive - d).abs() < 0.025
                    && (asymmetry - a).abs() < 0.025
                    && (bias - b).abs() < 0.0015
                {
                    return profile;
                }
            }
        }
        Self::Custom
    }
}

/// Holistic vocal character presets combining tube warmth, proximity,
/// presence articulation, air sheen, and dynamics compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VoiceCharacterProfile {
    // Dialogue & Voice
    /// Pure, natural, uncolored vocal baseline.
    #[default]
    #[serde(alias = "NaturalVoice")]
    Natural,
    /// Clean studio dialogue with subtle body and near-mic presence.
    StudioDialogue,
    /// Warm, intimate tone for long-form podcasts and comfortable listening.
    PodcastWarm,
    /// Articulate radio voice with punchy mids and crisp diction.
    FmVoice,
    /// Deep, cinematic voice with expansive warmth and rich lower body.
    CinematicVoice,
    /// Maximum intelligibility with transparent transient articulation.
    VoiceClarity,
    /// Tight newsreader delivery with authoritative attack and presence.
    NewsBroadcast,
    /// Soft, cozy, low-fatigue voice for narration, audiobooks, and storytelling.
    SoftStoryteller,
    /// Dynamic, commanding tone with pronounced harmonic drive.
    ConfidentSpeaker,
    /// Vintage broadcast tone with intentional tube/carbon-mic grit.
    VintageRadio,

    // Dominant & Authority
    /// Confident, calm, authoritative voice without harshness.
    Authority,
    /// Dense, deep-seated, assertive tone with low-end gravity.
    Dominant,
    /// Bold attack, intense presence, and commanding delivery.
    Commanding,
    /// Close, warm, but firmly controlling and persuasive tone.
    IntimateAuthority,
    /// Hard, aggressively expressive, commanding tone.
    HardAuthority,
    /// Dark, velvety, deeply saturated vocal weight.
    DarkVelvet,
    /// High-impact broadcast command with maximum articulation.
    CommandRadio,
    /// High-presence, commanding leader tone with heavy authority.
    DominantAuthority,

    // Character & Psychology
    /// Soft, warm, intimate, close-mic persuasive tone.
    #[serde(alias = "VelvetPersuasion")]
    VelvetCharmer,
    /// Dark, rich, confident charismatic voice.
    DarkCharisma,
    /// Clean, neutral surface tone with hidden depth and light polish.
    MaskedIntent,
    /// Quiet, dense, intimate near-mic commanding authority.
    WhisperAuthority,
    /// Playful, confident, punchy, expressive and provocative delivery.
    PlayfulControl,
    /// Cold, collected, emotionally controlled, assertive charisma.
    ColdCharisma,
    /// Dark, velvety, vintage antihero tone with rich harmonics.
    ElegantVillain,
    /// Smooth, soft, enveloping hypnotic vocal warmth.
    HypnoticWarmth,

    // Analog Coloration
    /// Class-A triode warmth with dominant 2nd harmonic and soft peak compression.
    GoldenVintage,
    /// Analog reel tape saturation with subtle glue and gentle harmonic blend.
    WarmTape,
    /// High-drive broadcast coloration reminiscent of pushed 12AX7 studio preamps.
    #[serde(alias = "HotTube")]
    HotTube,
    /// Ultra-transparent air and presence with minimal harmonic distortion.
    #[serde(alias = "SubtleAir")]
    SubtleAir,
    /// Thick vintage overdrive and lo-fi analog crunch.
    #[serde(alias = "HeavyDrive")]
    HeavyDrive,

    // Custom
    /// Custom user-tuned combination of tone, saturation, and dynamics parameters.
    Custom,
}

impl VoiceCharacterProfile {
    /// Dialogue and speech clarity profiles recommended for everyday conversation.
    pub const DIALOGUE: [Self; 10] = [
        Self::Natural,
        Self::StudioDialogue,
        Self::PodcastWarm,
        Self::FmVoice,
        Self::CinematicVoice,
        Self::VoiceClarity,
        Self::NewsBroadcast,
        Self::SoftStoryteller,
        Self::ConfidentSpeaker,
        Self::VintageRadio,
    ];

    /// Assertive, commanding, and authoritative vocal profiles.
    pub const DOMINANT: [Self; 8] = [
        Self::Authority,
        Self::Dominant,
        Self::Commanding,
        Self::IntimateAuthority,
        Self::HardAuthority,
        Self::DarkVelvet,
        Self::CommandRadio,
        Self::DominantAuthority,
    ];

    /// Dramatic, character, and psychological vocal tone profiles.
    pub const CHARACTER: [Self; 8] = [
        Self::VelvetCharmer,
        Self::DarkCharisma,
        Self::MaskedIntent,
        Self::WhisperAuthority,
        Self::PlayfulControl,
        Self::ColdCharisma,
        Self::ElegantVillain,
        Self::HypnoticWarmth,
    ];

    /// Analog vintage console, tape, and vacuum tube hardware emulation profiles.
    pub const ANALOG: [Self; 5] = [
        Self::GoldenVintage,
        Self::WarmTape,
        Self::HotTube,
        Self::SubtleAir,
        Self::HeavyDrive,
    ];

    /// All available character profile choices for UI presentation.
    pub const ALL: [Self; 32] = [
        Self::Natural,
        Self::StudioDialogue,
        Self::PodcastWarm,
        Self::FmVoice,
        Self::CinematicVoice,
        Self::VoiceClarity,
        Self::NewsBroadcast,
        Self::SoftStoryteller,
        Self::ConfidentSpeaker,
        Self::VintageRadio,
        Self::Authority,
        Self::Dominant,
        Self::Commanding,
        Self::IntimateAuthority,
        Self::HardAuthority,
        Self::DarkVelvet,
        Self::CommandRadio,
        Self::DominantAuthority,
        Self::VelvetCharmer,
        Self::DarkCharisma,
        Self::MaskedIntent,
        Self::WhisperAuthority,
        Self::PlayfulControl,
        Self::ColdCharisma,
        Self::ElegantVillain,
        Self::HypnoticWarmth,
        Self::GoldenVintage,
        Self::WarmTape,
        Self::HotTube,
        Self::SubtleAir,
        Self::HeavyDrive,
        Self::Custom,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Natural => "🗣️ Natural Voice (Baseline)",
            Self::StudioDialogue => "🎧 Studio Dialogue",
            Self::PodcastWarm => "🎙️ Podcast Warm",
            Self::FmVoice => "📻 FM Voice",
            Self::CinematicVoice => "🎬 Cinematic Voice",
            Self::VoiceClarity => "📞 Voice Clarity",
            Self::NewsBroadcast => "📰 News Broadcast",
            Self::SoftStoryteller => "🧸 Soft Storyteller",
            Self::ConfidentSpeaker => "🔥 Confident Speaker",
            Self::VintageRadio => "🕰️ Vintage Radio",
            Self::Authority => "👑 Authority",
            Self::Dominant => "🖤 Dominant",
            Self::Commanding => "⚡ Commanding",
            Self::IntimateAuthority => "🎙️ Intimate Authority",
            Self::HardAuthority => "🔥 Hard Authority",
            Self::DarkVelvet => "🥃 Dark Velvet",
            Self::CommandRadio => "📻 Command Radio",
            Self::DominantAuthority => "👑 Dominant Authority",
            Self::VelvetCharmer => "🐍 Velvet Charmer",
            Self::DarkCharisma => "🌑 Dark Charisma",
            Self::MaskedIntent => "🎭 Masked Intent",
            Self::WhisperAuthority => "🕯️ Whisper Authority",
            Self::PlayfulControl => "🐈 Playful Control",
            Self::ColdCharisma => "🖤 Cold Charisma",
            Self::ElegantVillain => "🎩 Elegant Villain",
            Self::HypnoticWarmth => "🔮 Hypnotic Warmth",
            Self::GoldenVintage => "🌟 Golden Vintage (Class-A)",
            Self::WarmTape => "📼 Warm Tape (Analog Reel)",
            Self::HotTube => "🎙️ Hot Tube (12AX7 Driven)",
            Self::SubtleAir => "🍃 Subtle Air (Clean Presence)",
            Self::HeavyDrive => "🎸 Heavy Drive (Lo-Fi Crunch)",
            Self::Custom => "⚙️ Custom",
        }
    }

    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Natural => "Максимально естественная речь, нейтральный баланс без лишнего окраса",
            Self::StudioDialogue => {
                "Чистый студийный голос, контролируемый близкий микрофон и собранный низ"
            }
            Self::PodcastWarm => {
                "Тёплый бархатный близкий тембр, комфортный для многочасового прослушивания"
            }
            Self::FmVoice => {
                "Чёткая дикция, собранная плотная середина и напористая радийная подача"
            }
            Self::CinematicVoice => {
                "Глубокий кинематографичный голос, объёмная грудная форманта и воздух"
            }
            Self::VoiceClarity => "Максимум разборчивости и артикуляции, срез низкочастотной мути",
            Self::NewsBroadcast => {
                "Собранный дикторский тембр, уверенная атака и эффект прямого эфира"
            }
            Self::SoftStoryteller => {
                "Мягкий, спокойный, интимный голос для повествования и историй"
            }
            Self::ConfidentSpeaker => {
                "Энергичный, напористый и убедительный лидерский тембр для выступлений"
            }
            Self::VintageRadio => {
                "Яркая ретро-окраска 40-х с тёплым ламповым характером и срезом краев"
            }
            Self::Authority => {
                "Уверенный, спокойный, властный тембр без излишней резкости и агрессии"
            }
            Self::Dominant => {
                "Плотный, низко воспринимаемый, напористый голос с весомым присутствием"
            }
            Self::Commanding => {
                "Яркая атака, пробивная разборчивость, бескомпромиссный командный тон"
            }
            Self::IntimateAuthority => {
                "Близкий, тёплый, контролирующий и психологически убедительный шепот"
            }
            Self::HardAuthority => "Жёсткий, агрессивно-выразительный и непоколебимый тембр силы",
            Self::DarkVelvet => {
                "Тёмный, насыщенный, бархатный глубокий голос с роскошными гармониками"
            }
            Self::CommandRadio => {
                "Дикторский, очень уверенный тактический радийный тон с четким приказом"
            }
            Self::DominantAuthority => {
                "Сильное присутствие, командный тембр, вес и максимальная лидерская уверенность"
            }
            Self::VelvetCharmer => {
                "Мягкий, тёплый, близкий тембр с выраженным доверительным эффектом приближения"
            }
            Self::DarkCharisma => "Тёмный, насыщенный, глубокий и магнетически уверенный тембр",
            Self::MaskedIntent => "Очень чистый, нейтральный и собранный звук со скрытым контролем",
            Self::WhisperAuthority => {
                "Близкий, плотный, тихо-уверенный тембр с максимальной интимностью и компрессией"
            }
            Self::PlayfulControl => {
                "Игривый, выразительный и яркий голос с открытым воздухом и напором"
            }
            Self::ColdCharisma => {
                "Холодный, собранный, эмоционально сдержанный голос хирургической точности"
            }
            Self::ElegantVillain => {
                "Тёмный, бархатный, винтажный антигеройский тембр с богатыми гармониками"
            }
            Self::HypnoticWarmth => {
                "Плавный, мягкий, обволакивающий гипнотический звук для убаюкивания"
            }
            Self::GoldenVintage => {
                "Классический триодный каскад класса А с богатой 2-й гармоникой и воздухом"
            }
            Self::WarmTape => {
                "Ленточная аналоговая компрессия Studer, мягкий низ и аналоговый клей"
            }
            Self::HotTube => "Пробивной радийный ламповый тон с ярким гармоническим телом (12AX7)",
            Self::SubtleAir => {
                "Прозрачный воздух 10 кГц и эффект присутствия без слышимого искажения"
            }
            Self::HeavyDrive => "Жирный аналоговый овердрайв, винтажный кранч и лоу-фай окрас",
            Self::Custom => "Пользовательская конфигурация эквалайзера, сатурации и динамики",
        }
    }

    /// Maps this character profile to its corresponding saturation profile.
    #[must_use]
    pub const fn to_saturation_profile(self) -> SaturationProfile {
        match self {
            Self::Natural => SaturationProfile::NaturalVoice,
            Self::StudioDialogue => SaturationProfile::StudioDialogue,
            Self::PodcastWarm => SaturationProfile::PodcastWarm,
            Self::FmVoice => SaturationProfile::FmVoice,
            Self::CinematicVoice => SaturationProfile::CinematicVoice,
            Self::VoiceClarity => SaturationProfile::VoiceClarity,
            Self::NewsBroadcast => SaturationProfile::NewsBroadcast,
            Self::SoftStoryteller => SaturationProfile::SoftStoryteller,
            Self::ConfidentSpeaker => SaturationProfile::ConfidentSpeaker,
            Self::VintageRadio => SaturationProfile::VintageRadio,
            Self::Authority => SaturationProfile::Authority,
            Self::Dominant => SaturationProfile::Dominant,
            Self::Commanding => SaturationProfile::Commanding,
            Self::IntimateAuthority => SaturationProfile::IntimateAuthority,
            Self::HardAuthority => SaturationProfile::HardAuthority,
            Self::DarkVelvet => SaturationProfile::DarkVelvet,
            Self::CommandRadio => SaturationProfile::CommandRadio,
            Self::DominantAuthority => SaturationProfile::DominantAuthority,
            Self::VelvetCharmer => SaturationProfile::VelvetPersuasion,
            Self::DarkCharisma => SaturationProfile::DarkCharisma,
            Self::MaskedIntent => SaturationProfile::MaskedIntent,
            Self::WhisperAuthority => SaturationProfile::WhisperAuthority,
            Self::PlayfulControl => SaturationProfile::PlayfulControl,
            Self::ColdCharisma => SaturationProfile::ColdCharisma,
            Self::ElegantVillain => SaturationProfile::ElegantVillain,
            Self::HypnoticWarmth => SaturationProfile::HypnoticWarmth,
            Self::GoldenVintage => SaturationProfile::GoldenVintage,
            Self::WarmTape => SaturationProfile::WarmTape,
            Self::HotTube => SaturationProfile::HotTube,
            Self::SubtleAir => SaturationProfile::SubtleAir,
            Self::HeavyDrive => SaturationProfile::HeavyDrive,
            Self::Custom => SaturationProfile::Custom,
        }
    }

    #[must_use]
    pub const fn params(self) -> Option<CharacterPresetParams> {
        match self {
            Self::Natural => Some(CharacterPresetParams {
                presence_db: 0.0,
                drive: 1.08,
                asymmetry: 1.15,
                bias: 0.000,
                proximity_db: 0.0,
                air_db: 0.0,
                compressor_ratio: 2.2,
                makeup_gain: 1.25,
            }),
            Self::StudioDialogue => Some(CharacterPresetParams {
                presence_db: 2.0,
                drive: 1.12,
                asymmetry: 1.35,
                bias: 0.000,
                proximity_db: 1.2,
                air_db: 1.5,
                compressor_ratio: 2.4,
                makeup_gain: 1.26,
            }),
            Self::PodcastWarm => Some(CharacterPresetParams {
                presence_db: 1.5,
                drive: 1.22,
                asymmetry: 1.65,
                bias: 0.001,
                proximity_db: 2.5,
                air_db: 0.8,
                compressor_ratio: 2.6,
                makeup_gain: 1.28,
            }),
            Self::FmVoice => Some(CharacterPresetParams {
                presence_db: 3.5,
                drive: 1.28,
                asymmetry: 1.55,
                bias: 0.002,
                proximity_db: 1.0,
                air_db: 1.5,
                compressor_ratio: 3.2,
                makeup_gain: 1.32,
            }),
            Self::CinematicVoice => Some(CharacterPresetParams {
                presence_db: 2.5,
                drive: 1.32,
                asymmetry: 1.72,
                bias: 0.002,
                proximity_db: 2.5,
                air_db: 2.0,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::VoiceClarity => Some(CharacterPresetParams {
                presence_db: 4.0,
                drive: 1.18,
                asymmetry: 1.25,
                bias: -0.001,
                proximity_db: -1.5,
                air_db: 1.0,
                compressor_ratio: 2.2,
                makeup_gain: 1.20,
            }),
            Self::NewsBroadcast => Some(CharacterPresetParams {
                presence_db: 3.5,
                drive: 1.40,
                asymmetry: 1.80,
                bias: 0.003,
                proximity_db: 1.2,
                air_db: 1.5,
                compressor_ratio: 3.5,
                makeup_gain: 1.35,
            }),
            Self::SoftStoryteller => Some(CharacterPresetParams {
                presence_db: 0.5,
                drive: 1.10,
                asymmetry: 1.50,
                bias: -0.002,
                proximity_db: 2.0,
                air_db: 0.5,
                compressor_ratio: 2.0,
                makeup_gain: 1.18,
            }),
            Self::ConfidentSpeaker => Some(CharacterPresetParams {
                presence_db: 3.0,
                drive: 1.38,
                asymmetry: 1.90,
                bias: 0.004,
                proximity_db: 1.5,
                air_db: 2.0,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::VintageRadio => Some(CharacterPresetParams {
                presence_db: 3.0,
                drive: 1.55,
                asymmetry: 2.00,
                bias: 0.006,
                proximity_db: -1.0,
                air_db: 0.0,
                compressor_ratio: 3.8,
                makeup_gain: 1.35,
            }),
            Self::Authority => Some(CharacterPresetParams {
                presence_db: 2.5,
                drive: 1.35,
                asymmetry: 1.75,
                bias: 0.002,
                proximity_db: 1.5,
                air_db: 1.0,
                compressor_ratio: 2.6,
                makeup_gain: 1.28,
            }),
            Self::Dominant => Some(CharacterPresetParams {
                presence_db: 3.0,
                drive: 1.50,
                asymmetry: 1.95,
                bias: 0.004,
                proximity_db: 2.5,
                air_db: 1.2,
                compressor_ratio: 3.0,
                makeup_gain: 1.32,
            }),
            Self::Commanding => Some(CharacterPresetParams {
                presence_db: 4.0,
                drive: 1.65,
                asymmetry: 2.10,
                bias: 0.005,
                proximity_db: 1.5,
                air_db: 2.0,
                compressor_ratio: 3.5,
                makeup_gain: 1.35,
            }),
            Self::IntimateAuthority => Some(CharacterPresetParams {
                presence_db: 2.2,
                drive: 1.28,
                asymmetry: 1.85,
                bias: 0.001,
                proximity_db: 3.0,
                air_db: 1.5,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::HardAuthority => Some(CharacterPresetParams {
                presence_db: 4.0,
                drive: 1.85,
                asymmetry: 2.20,
                bias: 0.007,
                proximity_db: 1.8,
                air_db: 2.0,
                compressor_ratio: 4.0,
                makeup_gain: 1.38,
            }),
            Self::DarkVelvet => Some(CharacterPresetParams {
                presence_db: 1.5,
                drive: 1.42,
                asymmetry: 2.15,
                bias: 0.006,
                proximity_db: 2.8,
                air_db: 1.0,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::CommandRadio => Some(CharacterPresetParams {
                presence_db: 3.8,
                drive: 1.58,
                asymmetry: 1.90,
                bias: 0.004,
                proximity_db: 1.0,
                air_db: 1.2,
                compressor_ratio: 3.5,
                makeup_gain: 1.35,
            }),
            Self::DominantAuthority => Some(CharacterPresetParams {
                presence_db: 3.5,
                drive: 1.45,
                asymmetry: 1.90,
                bias: 0.004,
                proximity_db: 2.0,
                air_db: 1.5,
                compressor_ratio: 3.0,
                makeup_gain: 1.32,
            }),
            Self::VelvetCharmer => Some(CharacterPresetParams {
                presence_db: 2.0,
                drive: 1.18,
                asymmetry: 1.55,
                bias: 0.001,
                proximity_db: 2.0,
                air_db: 1.0,
                compressor_ratio: 2.2,
                makeup_gain: 1.25,
            }),
            Self::DarkCharisma => Some(CharacterPresetParams {
                presence_db: 2.5,
                drive: 1.35,
                asymmetry: 1.85,
                bias: 0.003,
                proximity_db: 2.2,
                air_db: 1.2,
                compressor_ratio: 2.6,
                makeup_gain: 1.28,
            }),
            Self::MaskedIntent => Some(CharacterPresetParams {
                presence_db: 1.5,
                drive: 1.15,
                asymmetry: 1.45,
                bias: 0.000,
                proximity_db: 0.0,
                air_db: 0.5,
                compressor_ratio: 2.0,
                makeup_gain: 1.20,
            }),
            Self::WhisperAuthority => Some(CharacterPresetParams {
                presence_db: 2.5,
                drive: 1.25,
                asymmetry: 1.85,
                bias: 0.002,
                proximity_db: 3.0,
                air_db: 1.5,
                compressor_ratio: 3.0,
                makeup_gain: 1.32,
            }),
            Self::PlayfulControl => Some(CharacterPresetParams {
                presence_db: 3.5,
                drive: 1.28,
                asymmetry: 1.70,
                bias: 0.002,
                proximity_db: 1.0,
                air_db: 2.0,
                compressor_ratio: 2.5,
                makeup_gain: 1.28,
            }),
            Self::ColdCharisma => Some(CharacterPresetParams {
                presence_db: 3.0,
                drive: 1.30,
                asymmetry: 1.80,
                bias: 0.003,
                proximity_db: -1.0,
                air_db: 2.0,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::ElegantVillain => Some(CharacterPresetParams {
                presence_db: 2.5,
                drive: 1.45,
                asymmetry: 2.00,
                bias: 0.005,
                proximity_db: 2.0,
                air_db: 1.0,
                compressor_ratio: 2.8,
                makeup_gain: 1.30,
            }),
            Self::HypnoticWarmth => Some(CharacterPresetParams {
                presence_db: 1.2,
                drive: 1.18,
                asymmetry: 1.75,
                bias: 0.001,
                proximity_db: 2.5,
                air_db: 1.5,
                compressor_ratio: 2.2,
                makeup_gain: 1.22,
            }),
            Self::GoldenVintage => Some(CharacterPresetParams {
                presence_db: 1.5,
                drive: 1.15,
                asymmetry: 1.70,
                bias: 0.000,
                proximity_db: 1.0,
                air_db: 1.0,
                compressor_ratio: 2.2,
                makeup_gain: 1.25,
            }),
            Self::WarmTape => Some(CharacterPresetParams {
                presence_db: 1.0,
                drive: 1.30,
                asymmetry: 1.25,
                bias: 0.000,
                proximity_db: 1.8,
                air_db: 0.5,
                compressor_ratio: 2.5,
                makeup_gain: 1.28,
            }),
            Self::HotTube => Some(CharacterPresetParams {
                presence_db: 2.8,
                drive: 1.62,
                asymmetry: 2.05,
                bias: 0.005,
                proximity_db: 1.5,
                air_db: 1.5,
                compressor_ratio: 3.0,
                makeup_gain: 1.32,
            }),
            Self::SubtleAir => Some(CharacterPresetParams {
                presence_db: 2.0,
                drive: 1.05,
                asymmetry: 1.30,
                bias: 0.000,
                proximity_db: 0.0,
                air_db: 2.8,
                compressor_ratio: 2.0,
                makeup_gain: 1.20,
            }),
            Self::HeavyDrive => Some(CharacterPresetParams {
                presence_db: 3.0,
                drive: 2.20,
                asymmetry: 2.40,
                bias: 0.012,
                proximity_db: 1.0,
                air_db: 1.0,
                compressor_ratio: 3.5,
                makeup_gain: 1.35,
            }),
            Self::Custom => None,
        }
    }

    #[must_use]
    pub fn from_params(
        presence_db: f32,
        drive: f32,
        asymmetry: f32,
        proximity_db: f32,
        air_db: f32,
        ratio: f32,
    ) -> Self {
        let mut best = Self::Custom;
        let mut min_dist = f32::MAX;
        for profile in Self::ALL {
            if let Some(p) = profile.params() {
                let dist = (presence_db - p.presence_db).powi(2)
                    + (proximity_db - p.proximity_db).powi(2)
                    + (air_db - p.air_db).powi(2)
                    + (drive - p.drive).powi(2) * 16.0
                    + (asymmetry - p.asymmetry).powi(2) * 16.0
                    + (ratio - p.compressor_ratio).powi(2);
                if dist < min_dist {
                    min_dist = dist;
                    best = profile;
                }
            }
        }
        if min_dist < 0.25 { best } else { Self::Custom }
    }
}

/// Analog tube/tape vocal saturation processor.
///
/// Wraps [`mkaudiolibrary::dsp::Saturation`] — an asymmetric log₂ waveshaper
/// that generates the even harmonics (2nd, 4th) that give tube amplifiers and
/// tape their characteristic warmth — followed by a one-pole DC blocker.
///
/// The log₂ transfer function `β·log₂(1+α·x)/log₂(1+α)` is inherently more
/// musical than `tanh` for vocal material: it compresses peaks gently while
/// leaving low-level detail almost untouched.  Asymmetric α parameters
/// (positive side harder) introduce 2nd-harmonic bias similar to a class-A
/// tube stage.  A non-zero `bias` (δ) shifts the operating point off-centre,
/// further enriching even harmonics exactly as a biased triode does.
///
/// `drive` maps to the α parameter as `α = drive * 6.0`; `asymmetry` scales
/// α+ relative to α-; β is normalised for unity small-signal gain.
pub struct VocalWarmthProcessor {
    pub drive: f32,
    /// α+/α− ratio. 1.0 = symmetric, 1.7 = golden vintage, 2.5 = heavy tube.
    pub asymmetry: f32,
    /// DC bias offset δ (-0.02–0.05). Shifts operating point like class-A bias.
    pub bias: f32,
    pub enabled: bool,
    /// Cached mkaudiolibrary saturation — rebuilt whenever parameters change.
    sat: mkaudiolibrary::dsp::Saturation,
    /// Parameter snapshot used to detect when `sat` needs rebuilding.
    built_drive: f32,
    built_asymmetry: f32,
    built_bias: f32,
    dc_x1: f32,
    dc_y1: f32,
}

impl std::fmt::Debug for VocalWarmthProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VocalWarmthProcessor")
            .field("drive", &self.drive)
            .field("asymmetry", &self.asymmetry)
            .field("bias", &self.bias)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Clone for VocalWarmthProcessor {
    fn clone(&self) -> Self {
        Self::with_params(self.drive, self.asymmetry, self.bias, self.enabled)
    }
}

impl Default for VocalWarmthProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl VocalWarmthProcessor {
    /// Build the `mkaudiolibrary::dsp::Saturation` for the given parameters.
    ///
    /// - α− = `drive * 6.0`; α+ = α− × `asymmetry`
    /// - β normalised for unity slope at x=0: `β = ln2·log₂(1+α)/α`
    /// - δ = `bias` shifts the crossover point (class-A operating bias)
    fn make_sat(drive: f32, asymmetry: f32, bias: f32) -> mkaudiolibrary::dsp::Saturation {
        let d = drive.clamp(1.0, 3.0);
        let asym = asymmetry.clamp(1.0, 3.0);
        let b = bias.clamp(-0.02, 0.05);
        let alpha_n = d * 6.0;
        let alpha_p = alpha_n * asym;
        // Unity-slope normalisation: β = ln2 · log₂(1+α) / α
        let beta_p = std::f32::consts::LN_2 * (1.0 + alpha_p).log2() / alpha_p;
        let beta_n = std::f32::consts::LN_2 * (1.0 + alpha_n).log2() / alpha_n;
        mkaudiolibrary::dsp::Saturation::new(
            alpha_p, // α+ (positive side, harder knee)
            alpha_n, // α- (negative side)
            beta_p,  // β+ (unity-slope output gain)
            beta_n,  // β-
            b,       // δ  (class-A operating bias)
            false,   // no polarity flip
        )
    }

    /// Create a new processor configured with the specified profile.
    #[must_use]
    pub fn with_profile(profile: SaturationProfile) -> Self {
        let (d, a, b) = profile.params().unwrap_or((1.08, 1.15, 0.0));
        Self::with_params(d, a, b, true)
    }

    /// Apply profile parameters if not `Custom`.
    pub fn apply_profile(&mut self, profile: SaturationProfile) {
        if let Some((d, a, b)) = profile.params() {
            self.drive = d;
            self.asymmetry = a;
            self.bias = b;
        }
    }

    /// Default dialogue preset: Natural Voice (drive=1.08, asymmetry=1.15, bias=0.0).
    #[must_use]
    pub fn new() -> Self {
        Self::with_profile(SaturationProfile::NaturalVoice)
    }

    #[must_use]
    fn with_params(drive: f32, asymmetry: f32, bias: f32, enabled: bool) -> Self {
        let d = drive.clamp(1.0, 3.0);
        let asym = asymmetry.clamp(1.0, 3.0);
        let b = bias.clamp(-0.02, 0.05);
        Self {
            drive: d,
            asymmetry: asym,
            bias: b,
            enabled,
            sat: Self::make_sat(d, asym, b),
            built_drive: d,
            built_asymmetry: asym,
            built_bias: b,
            dc_x1: 0.0,
            dc_y1: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn process(&mut self, pcm: &mut [i16]) {
        if !self.enabled || pcm.is_empty() {
            return;
        }
        // Rebuild the saturation curve if any parameter changed since last call.
        let drive = self.drive.clamp(1.0, 3.0);
        let asym = self.asymmetry.clamp(1.0, 3.0);
        let bias = self.bias.clamp(-0.02, 0.05);
        if (drive - self.built_drive).abs() > 1e-4
            || (asym - self.built_asymmetry).abs() > 1e-4
            || (bias - self.built_bias).abs() > 1e-4
        {
            self.sat = Self::make_sat(drive, asym, bias);
            self.built_drive = drive;
            self.built_asymmetry = asym;
            self.built_bias = bias;
        }

        let r = 0.995_f32;
        let mut x1 = self.dc_x1;
        let mut y1 = self.dc_y1;

        for sample in pcm {
            let x = f32::from(*sample) / 32768.0;
            // mkaudiolibrary Saturation operates on normalised [-1, 1] f32.
            let saturated = self.sat.process(x);
            // One-pole DC blocker: y[n] = x[n] - x[n-1] + r·y[n-1]
            let blocked = saturated - x1 + r * y1;
            x1 = saturated;
            y1 = blocked;
            *sample = (blocked.clamp(-0.999, 0.999) * 32768.0).round() as i16;
        }
        self.dc_x1 = x1;
        self.dc_y1 = y1;
    }
}

pub struct MicrophoneDenoiseProcessor {
    denoiser: Box<nnnoiseless::DenoiseState<'static>>,
    input_resampler: LinearResampler,
    output_resampler: LinearResampler,
    resample_in: Vec<i16>,
    resample_out: Vec<i16>,
    fifo_in: Vec<f32>,
    fifo_out: Vec<i16>,
    frame_in: [f32; nnnoiseless::DenoiseState::FRAME_SIZE],
    frame_out: [f32; nnnoiseless::DenoiseState::FRAME_SIZE],
    frame_out_i16: [i16; nnnoiseless::DenoiseState::FRAME_SIZE],
    primed: bool,
    last_sample_rate: u32,
}

impl Default for MicrophoneDenoiseProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrophoneDenoiseProcessor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            denoiser: nnnoiseless::DenoiseState::new(),
            input_resampler: LinearResampler::new(),
            output_resampler: LinearResampler::new(),
            resample_in: Vec::with_capacity(1920),
            resample_out: Vec::with_capacity(1920),
            fifo_in: Vec::with_capacity(1920),
            fifo_out: Vec::with_capacity(1920),
            frame_in: [0.0; nnnoiseless::DenoiseState::FRAME_SIZE],
            frame_out: [0.0; nnnoiseless::DenoiseState::FRAME_SIZE],
            frame_out_i16: [0; nnnoiseless::DenoiseState::FRAME_SIZE],
            primed: false,
            last_sample_rate: 0,
        }
    }

    pub fn reset(&mut self) {
        self.denoiser = nnnoiseless::DenoiseState::new();
        self.input_resampler.reset();
        self.output_resampler.reset();
        self.resample_in.clear();
        self.resample_out.clear();
        self.fifo_in.clear();
        self.fifo_out.clear();
        self.primed = false;
        self.last_sample_rate = 0;
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn process(&mut self, pcm: &mut [i16], sample_rate: u32) {
        if pcm.is_empty() || sample_rate == 0 {
            return;
        }

        const FRAME_SIZE: usize = nnnoiseless::DenoiseState::FRAME_SIZE;

        if self.last_sample_rate != sample_rate {
            self.reset();
            self.last_sample_rate = sample_rate;
        }

        if !self.primed {
            let prime_samples =
                usize::try_from(u64::from(sample_rate).saturating_mul(FRAME_SIZE as u64) / 48_000)
                    .unwrap_or(FRAME_SIZE);
            self.fifo_out.resize(prime_samples, 0);
            self.primed = true;
        }

        if sample_rate == 48_000 {
            self.fifo_in.reserve(pcm.len());
            for &s in pcm.iter() {
                self.fifo_in.push(f32::from(s));
            }
            while self.fifo_in.len() >= FRAME_SIZE {
                self.frame_in.copy_from_slice(&self.fifo_in[..FRAME_SIZE]);
                self.fifo_in.drain(..FRAME_SIZE);
                self.denoiser
                    .process_frame(&mut self.frame_out, &self.frame_in);
                self.fifo_out.reserve(FRAME_SIZE);
                for &src in &self.frame_out {
                    self.fifo_out
                        .push(src.round().clamp(-32768.0, 32767.0) as i16);
                }
            }
        } else {
            self.input_resampler
                .resample_into(pcm, sample_rate, 48_000, &mut self.resample_in);
            self.fifo_in.reserve(self.resample_in.len());
            for &s in &self.resample_in {
                self.fifo_in.push(f32::from(s));
            }
            while self.fifo_in.len() >= FRAME_SIZE {
                self.frame_in.copy_from_slice(&self.fifo_in[..FRAME_SIZE]);
                self.fifo_in.drain(..FRAME_SIZE);
                self.denoiser
                    .process_frame(&mut self.frame_out, &self.frame_in);
                for (dest, &src) in self.frame_out_i16.iter_mut().zip(&self.frame_out) {
                    *dest = src.round().clamp(-32768.0, 32767.0) as i16;
                }
                self.output_resampler.resample_into(
                    &self.frame_out_i16,
                    48_000,
                    sample_rate,
                    &mut self.resample_out,
                );
                self.fifo_out.extend_from_slice(&self.resample_out);
            }
        }

        let count = pcm.len().min(self.fifo_out.len());
        pcm[..count].copy_from_slice(&self.fifo_out[..count]);
        self.fifo_out.drain(..count);
        if count < pcm.len() {
            pcm[count..].fill(0);
        }
    }
}

/// Parametric presence boost filter centered at 3.2 kHz to enhance vocal
/// articulation, authority, and intelligibility without adding harmonic distortion.
pub struct VoicePresenceFilter {
    biquad: mkaudiolibrary::dsp::iir::Biquad,
    pub boost_db: f32,
    sample_rate: u32,
}

impl std::fmt::Debug for VoicePresenceFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePresenceFilter")
            .field("boost_db", &self.boost_db)
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

impl Clone for VoicePresenceFilter {
    fn clone(&self) -> Self {
        let mut filter = Self::new(self.sample_rate);
        filter.set_boost_db(self.boost_db, self.sample_rate);
        filter
    }
}

impl Default for VoicePresenceFilter {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl VoicePresenceFilter {
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        Self {
            biquad: mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                3200.0,
                1.0,
                0.0,
            ),
            boost_db: 0.0,
            sample_rate: rate,
        }
    }

    pub fn set_boost_db(&mut self, boost_db: f32, sample_rate: u32) {
        let boost = boost_db.clamp(0.0, 6.0);
        let rate = sample_rate.max(1);
        if (boost - self.boost_db).abs() > 0.05 || rate != self.sample_rate {
            self.boost_db = boost;
            self.sample_rate = rate;
            self.biquad = mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                3200.0,
                1.0,
                boost,
            );
        }
    }

    pub fn reset(&mut self) {
        self.biquad.reset();
    }

    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn process(&mut self, pcm: &mut [i16]) {
        if self.boost_db <= 0.01 {
            return;
        }
        for sample in pcm.iter_mut() {
            let input = f32::from(*sample);
            let filtered = self.biquad.process(input);
            *sample = filtered.round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}

/// Parametric proximity EQ filter centered at 200 Hz (Q = 0.707) to adjust vocal
/// intimacy, chest resonance, and near-mic proximity effect.
pub struct VoiceProximityFilter {
    biquad: mkaudiolibrary::dsp::iir::Biquad,
    pub gain_db: f32,
    sample_rate: u32,
}

impl std::fmt::Debug for VoiceProximityFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceProximityFilter")
            .field("gain_db", &self.gain_db)
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

impl Clone for VoiceProximityFilter {
    fn clone(&self) -> Self {
        let mut filter = Self::new(self.sample_rate);
        filter.set_gain_db(self.gain_db, self.sample_rate);
        filter
    }
}

impl Default for VoiceProximityFilter {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl VoiceProximityFilter {
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        Self {
            biquad: mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                200.0,
                0.707,
                0.0,
            ),
            gain_db: 0.0,
            sample_rate: rate,
        }
    }

    pub fn set_gain_db(&mut self, gain_db: f32, sample_rate: u32) {
        let gain = gain_db.clamp(-3.0, 4.0);
        let rate = sample_rate.max(1);
        if (gain - self.gain_db).abs() > 0.05 || rate != self.sample_rate {
            self.gain_db = gain;
            self.sample_rate = rate;
            self.biquad = mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                200.0,
                0.707,
                gain,
            );
        }
    }

    pub fn reset(&mut self) {
        self.biquad.reset();
    }

    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn process(&mut self, pcm: &mut [i16]) {
        if self.gain_db.abs() <= 0.01 {
            return;
        }
        for sample in pcm.iter_mut() {
            let input = f32::from(*sample);
            let filtered = self.biquad.process(input);
            *sample = filtered.round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}

/// Parametric air EQ filter centered at 10 kHz (Q = 0.8) to add breathiness,
/// vocal openness, and silky high-frequency sheen without harshness.
pub struct VoiceAirFilter {
    biquad: mkaudiolibrary::dsp::iir::Biquad,
    pub boost_db: f32,
    sample_rate: u32,
}

impl std::fmt::Debug for VoiceAirFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceAirFilter")
            .field("boost_db", &self.boost_db)
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

impl Clone for VoiceAirFilter {
    fn clone(&self) -> Self {
        let mut filter = Self::new(self.sample_rate);
        filter.set_boost_db(self.boost_db, self.sample_rate);
        filter
    }
}

impl Default for VoiceAirFilter {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl VoiceAirFilter {
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        let center = 10000.0_f32.min((rate as f32) * 0.45);
        Self {
            biquad: mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                center,
                0.8,
                0.0,
            ),
            boost_db: 0.0,
            sample_rate: rate,
        }
    }

    pub fn set_boost_db(&mut self, boost_db: f32, sample_rate: u32) {
        let boost = boost_db.clamp(0.0, 4.0);
        let rate = sample_rate.max(1);
        if (boost - self.boost_db).abs() > 0.05 || rate != self.sample_rate {
            self.boost_db = boost;
            self.sample_rate = rate;
            let center = 10000.0_f32.min((rate as f32) * 0.45);
            self.biquad = mkaudiolibrary::dsp::iir::Biquad::new(
                mkaudiolibrary::dsp::iir::BiquadType::Peak,
                rate as f32,
                center,
                0.8,
                boost,
            );
        }
    }

    pub fn reset(&mut self) {
        self.biquad.reset();
    }

    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn process(&mut self, pcm: &mut [i16]) {
        // Air band at 10 kHz is above Nyquist for low sample rates (< 22.05 kHz).
        if self.boost_db <= 0.01 || self.sample_rate < 22_050 {
            return;
        }
        for sample in pcm.iter_mut() {
            let input = f32::from(*sample);
            let filtered = self.biquad.process(input);
            *sample = filtered.round().clamp(-32768.0, 32767.0) as i16;
        }
    }
}

pub struct MicrophoneProcessor {
    dynamics: AudioDynamicsProcessor,
    warmth: VocalWarmthProcessor,
    presence: VoicePresenceFilter,
    proximity: VoiceProximityFilter,
    air: VoiceAirFilter,
    denoiser: MicrophoneDenoiseProcessor,
    duck_gain: f32,
    duck_attack: f32,
    duck_release: f32,
    makeup_gain: f32,
    pub gain_percent: f32,
    pub dsp_enabled: bool,
    pub warmth_enabled: bool,
    pub echo_cancellation_enabled: bool,
    pub noise_suppression_enabled: bool,
    pub presence_boost_db: f32,
    pub proximity_gain_db: f32,
    pub air_boost_db: f32,
    pub voice_changer: crate::voice_changer::VoiceChangerProcessor,
}

impl MicrophoneProcessor {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate.max(1) as f32;
        Self {
            dynamics: AudioDynamicsProcessor::new(sample_rate),
            warmth: VocalWarmthProcessor::new(),
            presence: VoicePresenceFilter::new(sample_rate),
            proximity: VoiceProximityFilter::new(sample_rate),
            air: VoiceAirFilter::new(sample_rate),
            denoiser: MicrophoneDenoiseProcessor::new(),
            voice_changer: crate::voice_changer::VoiceChangerProcessor::new(),
            duck_gain: 1.0,
            duck_attack: 1.0 - (-1.0 / (0.002 * rate)).exp(),
            duck_release: 1.0 - (-1.0 / (0.080 * rate)).exp(),
            makeup_gain: 1.35,
            gain_percent: 100.0,
            dsp_enabled: true,
            warmth_enabled: true,
            echo_cancellation_enabled: true,
            noise_suppression_enabled: true,
            presence_boost_db: 0.0,
            proximity_gain_db: 0.0,
            air_boost_db: 0.0,
        }
    }

    pub fn configure_dynamics(
        &mut self,
        noise_gate_threshold: f32,
        compressor_threshold: f32,
        compressor_ratio: f32,
        makeup_gain: f32,
    ) {
        self.dynamics.noise_gate_threshold = noise_gate_threshold.clamp(0.0, 1.0);
        self.dynamics.compressor_threshold = compressor_threshold.clamp(0.0, 1.0);
        self.dynamics.compressor_ratio = compressor_ratio.clamp(1.0, 20.0);
        // Microphone makeup and user gain are applied together at the end so
        // one peak limiter can preserve headroom instead of clipping twice.
        self.makeup_gain = makeup_gain.clamp(0.0, 4.0);
        self.dynamics.makeup_gain = 1.0;
    }

    pub fn configure_warmth(&mut self, enabled: bool, drive: f32, asymmetry: f32, bias: f32) {
        self.warmth_enabled = enabled;
        self.warmth.enabled = enabled;
        self.warmth.drive = drive.clamp(1.0, 3.0);
        self.warmth.asymmetry = asymmetry.clamp(1.0, 3.0);
        self.warmth.bias = bias.clamp(-0.02, 0.05);
    }

    pub fn configure_presence(&mut self, boost_db: f32) {
        self.presence_boost_db = boost_db.clamp(0.0, 6.0);
        self.presence.set_boost_db(self.presence_boost_db, 48_000);
    }

    pub fn configure_proximity(&mut self, gain_db: f32) {
        self.proximity_gain_db = gain_db.clamp(-3.0, 4.0);
        self.proximity.set_gain_db(self.proximity_gain_db, 48_000);
    }

    pub fn configure_air(&mut self, boost_db: f32) {
        self.air_boost_db = boost_db.clamp(0.0, 4.0);
        self.air.set_boost_db(self.air_boost_db, 48_000);
    }

    pub fn configure_voice_changer(
        &mut self,
        enabled: bool,
        model_path: &str,
        pitch_shift: f32,
        wet: f32,
    ) {
        self.voice_changer.set_enabled(enabled);
        self.voice_changer.set_voice_model_path(model_path);
        self.voice_changer.set_pitch_shift(pitch_shift);
        self.voice_changer.set_wet(wet);
    }

    pub fn process(&mut self, pcm: &mut [i16], sample_rate: u32) {
        if sample_rate != self.dynamics_sample_rate() {
            self.dynamics.set_sample_rate(sample_rate);
            let rate = sample_rate.max(1) as f32;
            self.duck_attack = 1.0 - (-1.0 / (0.002 * rate)).exp();
            self.duck_release = 1.0 - (-1.0 / (0.080 * rate)).exp();
        }
        if self.echo_cancellation_enabled {
            // This is conservative echo suppression rather than a full
            // reference-signal AEC. Keep enough near-end speech for natural
            // full-duplex conversation while the speaker is active.
            let target = if speaker_is_active() { 0.35 } else { 1.0 };
            let coefficient = if target < self.duck_gain {
                self.duck_attack
            } else {
                self.duck_release
            };
            for sample in &mut *pcm {
                self.duck_gain += (target - self.duck_gain) * coefficient;
                *sample = (f32::from(*sample) * self.duck_gain)
                    .round()
                    .clamp(-32768.0, 32767.0) as i16;
            }
        }
        if self.noise_suppression_enabled {
            self.denoiser.process(pcm, sample_rate);
        } else if self.denoiser.primed {
            self.denoiser.reset();
        }
        if self.voice_changer.is_enabled() {
            // When voice changer is enabled, the model synthesizes full-fidelity audio.
            // Bypass mic character filters and dynamics/makeup gain to prevent unnatural
            // frequency bumps, compressor pumping, and digital clipping.
            self.warmth.reset();
            self.proximity.reset();
            self.presence.reset();
            self.air.reset();
            self.voice_changer.process(pcm, sample_rate);
        } else {
            if self.warmth_enabled {
                self.warmth.process(pcm);
            } else {
                self.warmth.reset();
            }
            if self.proximity_gain_db.abs() > 0.01 {
                self.proximity
                    .set_gain_db(self.proximity_gain_db, sample_rate);
                self.proximity.process(pcm);
            } else {
                self.proximity.reset();
            }
            if self.presence_boost_db > 0.01 {
                self.presence
                    .set_boost_db(self.presence_boost_db, sample_rate);
                self.presence.process(pcm);
            } else {
                self.presence.reset();
            }
            if self.air_boost_db > 0.01 {
                self.air.set_boost_db(self.air_boost_db, sample_rate);
                self.air.process(pcm);
            } else {
                self.air.reset();
            }
            if self.dsp_enabled {
                self.dynamics.process(pcm);
            }
            let makeup = if self.dsp_enabled {
                self.makeup_gain
            } else {
                1.0
            };
            let requested_gain = (if self.gain_percent.is_finite() {
                self.gain_percent
            } else {
                100.0
            })
            .clamp(25.0, 400.0)
                / 100.0
                * makeup;
            let peak = pcm
                .iter()
                .map(|sample| i32::from(*sample).unsigned_abs() as f32)
                .fold(0.0, f32::max);
            let gain = if peak > 0.0 {
                requested_gain.min((0.95 * 32768.0) / peak)
            } else {
                requested_gain
            };
            if (gain - 1.0).abs() > f32::EPSILON {
                for sample in pcm {
                    *sample = (f32::from(*sample) * gain).round().clamp(-32768.0, 32767.0) as i16;
                }
            }
        }
    }

    fn dynamics_sample_rate(&self) -> u32 {
        self.dynamics.sample_rate
    }
}

pub fn resample_to_8khz(pcm: &[i16], sample_rate: u32) -> Vec<i16> {
    resample_linear(pcm, sample_rate, VOICE_SAMPLE_RATE)
}

pub fn resample_pcm(pcm: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    resample_linear(pcm, input_rate, output_rate)
}

pub fn resample_pcm_into(pcm: &[i16], input_rate: u32, output_rate: u32, output: &mut Vec<i16>) {
    output.clear();
    if input_rate == 0 || output_rate == 0 {
        return;
    }
    if input_rate == output_rate || pcm.is_empty() {
        output.extend_from_slice(pcm);
        return;
    }
    let output_len = ((pcm.len() as u64 * output_rate as u64) / input_rate as u64) as usize;
    output.reserve(output_len);
    for index in 0..output_len {
        let source = index as f64 * input_rate as f64 / output_rate as f64;
        let left = source.floor() as usize;
        let right = (left + 1).min(pcm.len() - 1);
        let fraction = source - left as f64;
        output.push((pcm[left] as f64 * (1.0 - fraction) + pcm[right] as f64 * fraction) as i16);
    }
}

/// Stateful linear resampler that preserves fractional phase and edge samples
/// across chunk boundaries to avoid discontinuities (clicks) in continuous streams.
#[derive(Debug, Default, Clone)]
pub struct LinearResampler {
    last_sample: Option<i16>,
    phase: f64,
    last_input_rate: u32,
    last_output_rate: u32,
}

impl LinearResampler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_sample: None,
            phase: 0.0,
            last_input_rate: 0,
            last_output_rate: 0,
        }
    }

    pub fn reset(&mut self) {
        self.last_sample = None;
        self.phase = 0.0;
        self.last_input_rate = 0;
        self.last_output_rate = 0;
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn resample_into(
        &mut self,
        pcm: &[i16],
        input_rate: u32,
        output_rate: u32,
        output: &mut Vec<i16>,
    ) {
        output.clear();
        if input_rate == 0 || output_rate == 0 || pcm.is_empty() {
            return;
        }
        if input_rate != self.last_input_rate || output_rate != self.last_output_rate {
            self.reset();
            self.last_input_rate = input_rate;
            self.last_output_rate = output_rate;
        }
        if input_rate == output_rate {
            self.last_sample = pcm.last().copied();
            self.phase = 0.0;
            output.extend_from_slice(pcm);
            return;
        }

        let step = f64::from(input_rate) / f64::from(output_rate);
        let n = pcm.len();
        let n_f64 = n as f64;
        let mut pos = self.phase;

        let estimated = ((pcm.len() as u64 * output_rate as u64) / input_rate as u64) as usize + 2;
        output.reserve(estimated);

        if self.last_sample.is_none() {
            self.last_sample = pcm.first().copied();
            pos = 0.0;
        }

        if let Some(prev) = self.last_sample {
            let s0 = f64::from(prev);
            let s1 = f64::from(pcm[0]);
            while pos < 0.0 {
                let alpha = (pos + 1.0).clamp(0.0, 1.0);
                let val = (s0 * (1.0 - alpha) + s1 * alpha)
                    .round()
                    .clamp(-32768.0, 32767.0);
                output.push(val as i16);
                pos += step;
            }
        }

        let limit = (n.saturating_sub(1)) as f64;
        while pos < limit {
            let left = pos.floor() as usize;
            let right = left + 1;
            let frac = (pos - left as f64).clamp(0.0, 1.0);
            let s0 = f64::from(pcm[left]);
            let s1 = f64::from(pcm[right]);
            let val = (s0 * (1.0 - frac) + s1 * frac)
                .round()
                .clamp(-32768.0, 32767.0);
            output.push(val as i16);
            pos += step;
        }

        self.last_sample = pcm.last().copied();
        self.phase = pos - n_f64;
    }
}

fn resample_linear(pcm: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    let mut output = Vec::new();
    resample_pcm_into(pcm, input_rate, output_rate, &mut output);
    output
}

pub trait AudioSink: Send + Sync {
    fn write(&self, pcm: &[i16], sample_rate: u32);
}

struct TeeAudioSink {
    primary: Arc<dyn AudioSink>,
    secondary: Arc<dyn AudioSink>,
}

impl AudioSink for TeeAudioSink {
    fn write(&self, pcm: &[i16], sample_rate: u32) {
        self.primary.write(pcm, sample_rate);
        self.secondary.write(pcm, sample_rate);
    }
}

pub fn combine_audio_sinks(
    primary: Arc<dyn AudioSink>,
    secondary: Arc<dyn AudioSink>,
) -> Arc<dyn AudioSink> {
    Arc::new(TeeAudioSink { primary, secondary })
}

/// Applies the user-selected playback gain and optional speaker dynamics
/// before forwarding decoded peer audio to the platform sink.
pub struct ProcessedAudioSink {
    inner: Arc<dyn AudioSink>,
    state: Mutex<ProcessedAudioState>,
    gain: f32,
}

struct ProcessedAudioState {
    dynamics: AudioDynamicsProcessor,
    scratch: Vec<i16>,
    /// Gain applied to the last sample of the previous frame, so the next frame
    /// ramps from it instead of stepping at the frame boundary.
    last_gain: f32,
}

impl ProcessedAudioSink {
    pub fn new(
        inner: Arc<dyn AudioSink>,
        gain_percent: f32,
        dsp_enabled: bool,
        _makeup_gain: f32,
    ) -> Self {
        let mut dynamics = AudioDynamicsProcessor::new(VOICE_SAMPLE_RATE);
        dynamics.enabled = dsp_enabled;
        dynamics.noise_gate_threshold = 0.0;
        dynamics.compressor_threshold = 0.65;
        dynamics.compressor_ratio = 3.0;
        dynamics.makeup_gain = 1.0;
        let gain = (if gain_percent.is_finite() {
            gain_percent
        } else {
            100.0
        } / 100.0)
            .clamp(0.0, 2.0);
        Self {
            inner,
            state: Mutex::new(ProcessedAudioState {
                dynamics,
                scratch: Vec::new(),
                last_gain: gain,
            }),
            gain,
        }
    }
}

impl AudioSink for ProcessedAudioSink {
    fn write(&self, pcm: &[i16], sample_rate: u32) {
        if pcm.is_empty() {
            return;
        }
        note_speaker_activity(pcm);
        let mut previous_gain = self.gain;
        let mut samples = if let Ok(mut state) = self.state.lock() {
            let mut scratch = std::mem::take(&mut state.scratch);
            scratch.clear();
            scratch.extend_from_slice(pcm);
            if sample_rate != state.dynamics.sample_rate {
                state.dynamics.set_sample_rate(sample_rate);
            }
            state.dynamics.process(&mut scratch);
            previous_gain = state.last_gain;
            scratch
        } else {
            pcm.to_vec()
        };
        let peak = samples
            .iter()
            .map(|sample| i32::from(*sample).unsigned_abs() as f32)
            .fold(0.0, f32::max);
        let max_allowed = 0.95 * 32768.0;
        let target_gain = if peak > 0.0 && peak * self.gain > max_allowed {
            max_allowed / peak
        } else {
            self.gain
        };
        let mut applied_gain = target_gain;
        if (target_gain - 1.0).abs() > f32::EPSILON || (previous_gain - 1.0).abs() > f32::EPSILON {
            // Ramp the gain linearly across the frame instead of stepping it at the
            // frame boundary (a step is audible as a click). Each sample is still
            // hard-bounded so the ramp can never overshoot the limiter ceiling.
            let len = samples.len().max(1) as f32;
            for (index, sample) in samples.iter_mut().enumerate() {
                let t = (index + 1) as f32 / len;
                let mut gain = previous_gain + (target_gain - previous_gain) * t;
                let magnitude = f32::from(*sample).abs();
                if magnitude * gain > max_allowed {
                    gain = max_allowed / magnitude;
                }
                applied_gain = gain;
                *sample = (f32::from(*sample) * gain).round().clamp(-32768.0, 32767.0) as i16;
            }
        }
        if let Ok(mut state) = self.state.lock() {
            state.last_gain = applied_gain;
        }
        self.inner.write(&samples, sample_rate);
        samples.clear();
        if let Ok(mut state) = self.state.lock()
            && state.scratch.capacity() < samples.capacity()
        {
            state.scratch = samples;
        }
    }
}

pub trait MicrophoneCapture {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDevices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[cfg(feature = "cpal-audio")]
fn device_name(device: &cpal::Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|description| description.name().to_owned())
}

#[cfg(feature = "cpal-audio")]
#[must_use]
pub fn audio_devices() -> AudioDevices {
    let host = cpal::default_host();
    let mut inputs = host
        .input_devices()
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|device| device_name(&device))
        .collect::<Vec<_>>();
    let mut outputs = host
        .output_devices()
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|device| device_name(&device))
        .collect::<Vec<_>>();
    inputs.sort_unstable();
    inputs.dedup();
    outputs.sort_unstable();
    outputs.dedup();
    AudioDevices {
        inputs,
        outputs,
        default_input: host
            .default_input_device()
            .and_then(|device| device_name(&device)),
        default_output: host
            .default_output_device()
            .and_then(|device| device_name(&device)),
    }
}

#[cfg(not(feature = "cpal-audio"))]
#[must_use]
pub fn audio_devices() -> AudioDevices {
    AudioDevices::default()
}

#[cfg(feature = "cpal-audio")]
fn select_device<I>(
    devices: Result<I, cpal::Error>,
    default: Option<cpal::Device>,
    preferred: Option<&str>,
    kind: &str,
) -> Result<(cpal::Device, bool), String>
where
    I: Iterator<Item = cpal::Device>,
{
    let preferred = preferred.map(str::trim).filter(|name| !name.is_empty());
    if let Some(preferred) = preferred
        && let Ok(devices) = devices
    {
        for device in devices {
            if device_name(&device).as_deref() == Some(preferred) {
                return Ok((device, false));
            }
        }
    }
    default
        .map(|device| (device, preferred.is_some()))
        .ok_or_else(|| format!("no default {kind}"))
}

#[derive(Default)]
pub struct NullAudioSink;
impl AudioSink for NullAudioSink {
    fn write(&self, _pcm: &[i16], _sample_rate: u32) {}
}

pub struct WavAudioSink {
    sender: Option<mpsc::SyncSender<WavMessage>>,
    recycled: Mutex<mpsc::Receiver<Vec<i16>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum WavMessage {
    Audio(Vec<i16>),
    Finish,
}

impl WavAudioSink {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = File::create(path)?;
        file.write_all(&[0; 44])?;
        let (sender, receiver) = mpsc::sync_channel(32);
        let (recycle_sender, recycled) = mpsc::sync_channel(4);
        for _ in 0..4 {
            let _ = recycle_sender.try_send(Vec::with_capacity(5_760));
        }
        let thread = std::thread::Builder::new()
            .name("nektome-wav-writer".into())
            .spawn(move || run_wav_writer(file, &receiver, &recycle_sender))?;
        Ok(Self {
            sender: Some(sender),
            recycled: Mutex::new(recycled),
            thread: Some(thread),
        })
    }
}

impl AudioSink for WavAudioSink {
    fn write(&self, pcm: &[i16], sample_rate: u32) {
        if pcm.is_empty() || sample_rate == 0 {
            return;
        }
        let Some(mut resampled) = self
            .recycled
            .lock()
            .ok()
            .and_then(|receiver| receiver.try_recv().ok())
        else {
            return;
        };
        resample_pcm_into(pcm, sample_rate, VOICE_SAMPLE_RATE, &mut resampled);
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.try_send(WavMessage::Audio(resampled));
        }
    }
}

fn run_wav_writer(
    mut file: File,
    receiver: &mpsc::Receiver<WavMessage>,
    recycled: &mpsc::SyncSender<Vec<i16>>,
) {
    let mut data_bytes = 0u32;
    let mut bytes = Vec::with_capacity(5_760 * 2);
    while let Ok(message) = receiver.recv() {
        let WavMessage::Audio(mut pcm) = message else {
            break;
        };
        bytes.clear();
        bytes.reserve(pcm.len().saturating_mul(2));
        for sample in &pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        if file.write_all(&bytes).is_ok() {
            data_bytes = data_bytes.saturating_add(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        }
        pcm.clear();
        let _ = recycled.try_send(pcm);
    }
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(b"RIFF");
    let _ = file.write_all(&(36u32.saturating_add(data_bytes)).to_le_bytes());
    let _ = file.write_all(b"WAVEfmt ");
    let _ = file.write_all(&16u32.to_le_bytes());
    let _ = file.write_all(&1u16.to_le_bytes());
    let _ = file.write_all(&1u16.to_le_bytes());
    let _ = file.write_all(&VOICE_SAMPLE_RATE.to_le_bytes());
    let _ = file.write_all(&(VOICE_SAMPLE_RATE * 2).to_le_bytes());
    let _ = file.write_all(&2u16.to_le_bytes());
    let _ = file.write_all(&16u16.to_le_bytes());
    let _ = file.write_all(b"data");
    let _ = file.write_all(&data_bytes.to_le_bytes());
    let _ = file.flush();
}

impl Drop for WavAudioSink {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(WavMessage::Finish);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "cpal-audio")]
pub struct CpalMicrophone {
    stream: Option<Stream>,
    callback: Arc<dyn Fn(&mut [i16], u32) + Send + Sync>,
    preferred_device: Option<String>,
    description: Option<String>,
}

#[cfg(feature = "cpal-audio")]
impl CpalMicrophone {
    pub fn new(callback: impl Fn(&mut [i16], u32) + Send + Sync + 'static) -> Self {
        Self::new_with_device(None, callback)
    }

    pub fn new_with_device(
        preferred_device: Option<String>,
        callback: impl Fn(&mut [i16], u32) + Send + Sync + 'static,
    ) -> Self {
        Self {
            stream: None,
            callback: Arc::new(callback),
            preferred_device,
            description: None,
        }
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn set_preferred_device(&mut self, preferred_device: Option<String>) {
        self.preferred_device = preferred_device;
    }
}

#[cfg(feature = "cpal-audio")]
impl MicrophoneCapture for CpalMicrophone {
    fn start(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        let host = cpal::default_host();
        let (device, used_fallback) = select_device(
            host.input_devices(),
            host.default_input_device(),
            self.preferred_device.as_deref(),
            "microphone",
        )?;
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        if config.channels == 0 || config.sample_rate == 0 {
            return Err("microphone reports invalid channel count or sample rate".into());
        }
        let rate = config.sample_rate;
        let callback = self.callback.clone();
        let error = |e| eprintln!("microphone stream error: {e}");
        let stream = match sample_format {
            SampleFormat::I16 => build_input::<i16>(&device, &config, callback, error, rate),
            SampleFormat::U16 => build_input::<u16>(&device, &config, callback, error, rate),
            SampleFormat::F32 => build_input::<f32>(&device, &config, callback, error, rate),
            other => return Err(format!("unsupported microphone format: {other:?}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        let device_name = device_name(&device).unwrap_or_else(|| "Default microphone".into());
        let fallback = if used_fallback {
            " · saved device unavailable; using default"
        } else {
            ""
        };
        self.description = Some(
            format!(
                "{device_name} · {} Hz · {} channel{} · {sample_format:?}",
                config.sample_rate,
                config.channels,
                if config.channels == 1 { "" } else { "s" }
            ) + fallback,
        );
        self.stream = Some(stream);
        Ok(())
    }
    fn stop(&mut self) {
        self.stream = None;
    }
}

#[cfg(feature = "cpal-audio")]
fn build_input<T: SizedSample + Send + 'static>(
    device: &cpal::Device,
    config: &StreamConfig,
    callback: Arc<dyn Fn(&mut [i16], u32) + Send + Sync>,
    error: impl FnMut(cpal::Error) + Send + 'static,
    rate: u32,
) -> Result<Stream, cpal::Error>
where
    T: Sample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    // Most native devices deliver far fewer than 4,096 frames per callback.
    // Reserving once keeps normal callback execution allocation-free.
    let mut pcm = Vec::with_capacity(4_096);
    device.build_input_stream(
        *config,
        move |data: &[T], _| {
            pcm.clear();
            pcm.reserve(data.len() / channels);
            pcm.extend(data.chunks(channels).map(|frame| {
                frame[0]
                    .to_sample::<f32>()
                    .clamp(-1.0, 1.0)
                    .mul_add(32767.0, 0.0) as i16
            }));
            callback(&mut pcm, rate);
        },
        error,
        None,
    )
}

pub struct QueuedAudioSink {
    queue: Arc<Mutex<VecDeque<i16>>>,
}

pub fn default_sink(
    speaker_gain_percent: f32,
    speaker_dsp_enabled: bool,
    makeup_gain: f32,
    wav_path: Option<&str>,
    output_device: Option<&str>,
) -> Result<Arc<dyn AudioSink>, String> {
    let wav_path = wav_path
        .filter(|path| !path.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("NEKTOME_REMOTE_AUDIO_WAV").ok())
        .filter(|path| !path.trim().is_empty());
    let sink: Arc<dyn AudioSink> = {
        #[cfg(feature = "cpal-audio")]
        {
            let recorder = wav_path
                .as_deref()
                .map(|path| {
                    WavAudioSink::create(path.trim())
                        .map(|sink| Arc::new(sink) as Arc<dyn AudioSink>)
                        .map_err(|error| error.to_string())
                })
                .transpose()?;
            let speaker = CpalAudioSink::new_with_device(output_device.map(str::to_owned))
                .ok()
                .map(|sink| Arc::new(sink) as Arc<dyn AudioSink>);
            match (speaker, recorder) {
                (Some(speaker), Some(recorder)) => Arc::new(TeeAudioSink {
                    primary: speaker,
                    secondary: recorder,
                }),
                (Some(speaker), None) => speaker,
                (None, Some(recorder)) => recorder,
                (None, None) => Arc::new(QueuedAudioSink::new()),
            }
        }

        #[cfg(not(feature = "cpal-audio"))]
        {
            let _ = output_device;
            if let Some(path) = wav_path {
                Arc::new(WavAudioSink::create(path.trim()).map_err(|error| error.to_string())?)
            } else {
                Arc::new(QueuedAudioSink::new())
            }
        }
    };
    Ok(Arc::new(ProcessedAudioSink::new(
        sink,
        speaker_gain_percent,
        speaker_dsp_enabled,
        makeup_gain,
    )))
}

#[cfg(feature = "cpal-audio")]
pub struct CpalAudioSink {
    queue: Arc<Mutex<VecDeque<i16>>>,
    scratch: Mutex<Vec<i16>>,
    resampler: Mutex<LinearResampler>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    output_rate: u32,
}

#[cfg(feature = "cpal-audio")]
impl CpalAudioSink {
    pub fn new() -> Result<Self, String> {
        Self::new_with_device(None)
    }

    fn new_with_device(preferred_device: Option<String>) -> Result<Self, String> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let thread_queue = Arc::clone(&queue);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("nektome-speaker".into())
            .spawn(move || {
                let setup = (|| -> Result<(Stream, u32), String> {
                    let host = cpal::default_host();
                    let (device, _) = select_device(
                        host.output_devices(),
                        host.default_output_device(),
                        preferred_device.as_deref(),
                        "speaker",
                    )?;
                    let supported = device.default_output_config().map_err(|e| e.to_string())?;
                    let sample_format = supported.sample_format();
                    let config: StreamConfig = supported.into();
                    if config.channels == 0 || config.sample_rate == 0 {
                        return Err("speaker reports invalid channel count or sample rate".into());
                    }
                    let output_rate = config.sample_rate;
                    let callback_queue = Arc::clone(&thread_queue);
                    let error = |e| eprintln!("speaker stream error: {e}");
                    let stream = match sample_format {
                        SampleFormat::I16 => {
                            build_output::<i16>(&device, &config, callback_queue, error)
                        }
                        SampleFormat::U16 => {
                            build_output::<u16>(&device, &config, callback_queue, error)
                        }
                        SampleFormat::F32 => {
                            build_output::<f32>(&device, &config, callback_queue, error)
                        }
                        other => return Err(format!("unsupported speaker format: {other:?}")),
                    }
                    .map_err(|e| e.to_string())?;
                    stream.play().map_err(|e| e.to_string())?;
                    Ok((stream, output_rate))
                })();
                match setup {
                    Ok((stream, output_rate)) => {
                        let _ = ready_tx.send(Ok(output_rate));
                        let _ = shutdown_rx.recv();
                        drop(stream);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| format!("could not start speaker thread: {error}"))?;
        let output_rate = ready_rx
            .recv()
            .map_err(|_| "speaker thread stopped during startup".to_owned())??;
        Ok(Self {
            queue,
            scratch: Mutex::new(Vec::new()),
            resampler: Mutex::new(LinearResampler::new()),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
            output_rate,
        })
    }
}

#[cfg(feature = "cpal-audio")]
impl Drop for CpalAudioSink {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Nudges the playout queue back towards its target depth by stretching or
/// squeezing the incoming frame by a few samples, spread evenly over the whole
/// frame with linear interpolation. The first and last samples are preserved, so
/// there is no discontinuity at the frame boundary (the old implementation
/// dropped/duplicated one sample mid-frame, a click every 20 ms while the queue
/// was outside the window). A 960-sample frame changed by 1..=4 samples is a
/// 0.1-0.4 % pitch change, which is inaudible.
#[cfg(feature = "cpal-audio")]
fn correct_playback_drift(
    samples: &mut Vec<i16>,
    queued_samples: usize,
    target_samples: usize,
    threshold: usize,
) {
    let len = samples.len();
    if len < 64 {
        return;
    }
    let after_enqueue = queued_samples.saturating_add(len);
    let high = target_samples.saturating_add(threshold);
    let low = target_samples.saturating_sub(threshold);
    let (excess, shrink) = if after_enqueue > high {
        (after_enqueue - high, true)
    } else if after_enqueue < low {
        (low - after_enqueue, false)
    } else {
        return;
    };
    // Proportional correction: remove/add up to 8 % of the frame length per
    // call so large backlogs drain in seconds rather than minutes.  A 4-sample
    // hard cap at 48 kHz / 50 fps was only ~4 ms/s of drain — far too slow for
    // an excess of 400+ ms.  Cap at 8 % (≈76 samples on a 956-sample frame)
    // and clamp to at least 1.
    let max_step = (len / 12).max(1);
    let step = (1 + excess / (len * 2)).min(max_step).min(len / 16).max(1);
    let new_len = if shrink {
        len.saturating_sub(step)
    } else {
        len + step
    };
    if new_len == len || new_len == 0 {
        return;
    }
    let scale = (len - 1) as f64 / (new_len - 1) as f64;
    let mut output = Vec::with_capacity(new_len);
    for index in 0..new_len {
        let source = index as f64 * scale;
        let left = source.floor() as usize;
        let right = (left + 1).min(len - 1);
        let fraction = source - left as f64;
        let value =
            f64::from(samples[left]) * (1.0 - fraction) + f64::from(samples[right]) * fraction;
        output.push(value.round() as i16);
    }
    *samples = output;
}

#[cfg(feature = "cpal-audio")]
impl AudioSink for CpalAudioSink {
    fn write(&self, pcm: &[i16], sample_rate: u32) {
        let mut samples = if let Ok(mut scratch) = self.scratch.lock() {
            let mut samples = std::mem::take(&mut *scratch);
            if let Ok(mut resampler) = self.resampler.lock() {
                resampler.resample_into(pcm, sample_rate, self.output_rate, &mut samples);
            } else {
                resample_pcm_into(pcm, sample_rate, self.output_rate, &mut samples);
            }
            samples
        } else {
            resample_linear(pcm, sample_rate, self.output_rate)
        };
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        let target_samples = usize::try_from(
            u64::from(self.output_rate)
                .saturating_mul(u64::from(SPEAKER_TARGET_BUFFER_MS.load(Ordering::Relaxed)))
                / 1_000,
        )
        .unwrap_or(usize::MAX);
        let correction_threshold = usize::try_from(self.output_rate / 16).unwrap_or(1).max(1);
        if SPEAKER_PLAYOUT_STARTED.load(Ordering::Relaxed) {
            correct_playback_drift(
                &mut samples,
                queue.len(),
                target_samples,
                correction_threshold,
            );
        }
        if audio_debug() {
            use std::sync::atomic::{AtomicI32, AtomicU64};
            static LAST_SAMPLE: AtomicI32 = AtomicI32::new(0);
            static LAST_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
            static WRITES: AtomicU64 = AtomicU64::new(0);
            if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
                let boundary = (i32::from(*first) - LAST_SAMPLE.load(Ordering::Relaxed)).abs();
                let inner = samples
                    .windows(2)
                    .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
                    .max()
                    .unwrap_or(0);
                if boundary > 6_000 || inner > 6_000 {
                    audio_debug_log(|| {
                        format!(
                            "JUMP in data entering device queue: frame-boundary={boundary} max-inner-step={inner} len={} in={sample_rate}Hz out={}Hz",
                            samples.len(),
                            self.output_rate
                        )
                    });
                }
                LAST_SAMPLE.store(i32::from(*last), Ordering::Relaxed);
            }
            let underruns = SPEAKER_UNDERRUNS.load(Ordering::Relaxed);
            if underruns != LAST_UNDERRUNS.swap(underruns, Ordering::Relaxed) {
                audio_debug_log(|| {
                    format!(
                        "UNDERRUN (total {underruns}), queue={}ms",
                        queue.len() * 1000 / self.output_rate as usize
                    )
                });
            }
            if WRITES.fetch_add(1, Ordering::Relaxed) % 100 == 0 {
                audio_debug_log(|| {
                    format!(
                        "queue={}ms target={}ms in={sample_rate}Hz out={}Hz",
                        queue.len() * 1000 / self.output_rate as usize,
                        target_samples * 1000 / self.output_rate as usize,
                        self.output_rate
                    )
                });
            }
        }
        queue.extend(samples.iter().copied());
        // Hard cap: drop the oldest samples when the queue grows beyond target +
        // 150 ms.  The old cap (target + 500 ms) let the queue stabilise at
        // ~550 ms, never returning to target even with drift correction running.
        let maximum_samples = target_samples
            .saturating_add(usize::try_from(self.output_rate * 15 / 100).unwrap_or(usize::MAX));
        if queue.len() > maximum_samples {
            let excess = queue.len() - maximum_samples;
            queue.drain(..excess);
        }
        drop(queue);
        samples.clear();
        if let Ok(mut scratch) = self.scratch.lock()
            && scratch.capacity() < samples.capacity()
        {
            *scratch = samples;
        }
    }
}

#[cfg(feature = "cpal-audio")]
fn build_output<T: SizedSample + FromSample<f32> + Send + 'static>(
    device: &cpal::Device,
    config: &StreamConfig,
    queue: Arc<Mutex<VecDeque<i16>>>,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, cpal::Error> {
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    let mut playing = false;
    let mut underrunning = false;
    let mut underrun_fade: u8 = 64;
    let mut consecutive_empty: usize = 0;
    let mut last_sample: f32 = 0.0;
    device.build_output_stream(
        *config,
        move |data: &mut [T], _| {
            let Ok(mut queue) = queue.lock() else {
                for sample in data.iter_mut() {
                    *sample = T::from_sample(0.0);
                }
                return;
            };
            if !SPEAKER_PLAYOUT_STARTED.load(Ordering::Relaxed) {
                playing = false;
            }
            let target_ms = SPEAKER_TARGET_BUFFER_MS
                .load(Ordering::Relaxed)
                .clamp(40, 200);
            let prebuffer_samples =
                usize::try_from(u64::from(rate).saturating_mul(u64::from(target_ms)) / 1000)
                    .unwrap_or(2880);

            if !playing {
                if queue.len() >= prebuffer_samples {
                    playing = true;
                    underrunning = false;
                    underrun_fade = 64;
                    consecutive_empty = 0;
                    SPEAKER_PLAYOUT_STARTED.store(true, Ordering::Relaxed);
                } else {
                    last_sample = 0.0;
                    for sample in data.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                    return;
                }
            }

            let muted = SPEAKER_MUTED.load(Ordering::Relaxed);
            for frame in data.chunks_mut(channels) {
                let sample_f32 = if let Some(sample) = queue.pop_front() {
                    let raw = f32::from(sample) / 32768.0;
                    let s = if underrunning {
                        if underrun_fade < 64 {
                            underrun_fade = underrun_fade.saturating_add(1);
                            let ramp = f32::from(underrun_fade) / 64.0;
                            last_sample * (1.0 - ramp) + raw * ramp
                        } else {
                            underrunning = false;
                            raw
                        }
                    } else {
                        raw
                    };
                    consecutive_empty = 0;
                    last_sample = s;
                    s
                } else {
                    if !underrunning {
                        SPEAKER_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                        underrunning = true;
                        underrun_fade = 0;
                    }
                    consecutive_empty = consecutive_empty.saturating_add(1);
                    let max_empty = usize::try_from(rate)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(150)
                        / 1000;
                    if consecutive_empty > max_empty {
                        playing = false;
                    }
                    last_sample *= 0.96;
                    last_sample
                };
                let sample_val = if muted { 0.0 } else { sample_f32 };
                let value = T::from_sample(sample_val);
                for channel in frame {
                    *channel = value;
                }
            }
        },
        error,
        None,
    )
}

impl QueuedAudioSink {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    pub fn queued_samples(&self) -> usize {
        self.queue.lock().map_or(0, |queue| queue.len())
    }
    pub fn take(&self, count: usize) -> Vec<i16> {
        let Ok(mut queue) = self.queue.lock() else {
            return Vec::new();
        };
        (0..count).filter_map(|_| queue.pop_front()).collect()
    }
}

impl Default for QueuedAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for QueuedAudioSink {
    fn write(&self, pcm: &[i16], _sample_rate: u32) {
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        queue.extend(pcm.iter().copied());
        if queue.len() > 80_000 {
            let excess = queue.len() - 80_000;
            queue.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_sink_preserves_pcm_order() {
        let sink = QueuedAudioSink::new();
        sink.write(&[1, 2, 3], VOICE_SAMPLE_RATE);
        assert_eq!(sink.take(2), vec![1, 2]);
        assert_eq!(sink.take(2), vec![3]);
    }

    #[cfg(feature = "cpal-audio")]
    #[test]
    fn playback_drift_correction_stretches_frame_without_discontinuities() {
        let original: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();

        let mut low_queue = original.clone();
        correct_playback_drift(&mut low_queue, 0, 10_000, 100);
        assert!(low_queue.len() > original.len());
        assert_eq!(low_queue.first(), original.first());
        assert_eq!(low_queue.last(), original.last());
        assert!(
            low_queue
                .windows(2)
                .all(|w| w[1] >= w[0] && w[1] - w[0] <= 11)
        );

        let mut high_queue = original.clone();
        correct_playback_drift(&mut high_queue, 20_000, 10_000, 100);
        assert!(high_queue.len() < original.len());
        assert_eq!(high_queue.first(), original.first());
        assert_eq!(high_queue.last(), original.last());
        assert!(
            high_queue
                .windows(2)
                .all(|w| w[1] >= w[0] && w[1] - w[0] <= 11)
        );

        let mut in_range_queue = original.clone();
        correct_playback_drift(&mut in_range_queue, 9_100, 10_000, 100);
        assert_eq!(in_range_queue, original);
    }

    #[test]
    fn processed_sink_ramps_gain_between_frames() {
        let inner = Arc::new(QueuedAudioSink::new());
        let sink = ProcessedAudioSink::new(inner.clone(), 200.0, false, 1.0);
        sink.write(&[1_000; 4], VOICE_SAMPLE_RATE);
        sink.write(&[30_000; 4], VOICE_SAMPLE_RATE);
        let output = inner.take(8);
        assert!(output[4..].iter().all(|s| *s <= 31_130));
        assert!(output[3] < output[4] + 1);
    }

    #[test]
    fn processed_sink_applies_playback_gain() {
        let inner = Arc::new(QueuedAudioSink::new());
        let sink = ProcessedAudioSink::new(inner.clone(), 50.0, false, 2.0);
        sink.write(&[1_000, -1_000], VOICE_SAMPLE_RATE);
        assert_eq!(inner.take(2), vec![500, -500]);
    }

    #[test]
    fn processed_sink_limits_loud_signals_without_clipping() {
        let inner = Arc::new(QueuedAudioSink::new());
        let sink = ProcessedAudioSink::new(inner.clone(), 200.0, true, 1.0);
        sink.write(&[30_000, -30_000], VOICE_SAMPLE_RATE);
        let output = inner.take(2);
        assert!(
            output[0] <= 31_200,
            "Output should be limited below full scale clipping"
        );
        assert!(
            output[1] >= -31_200,
            "Negative output should be limited above negative clipping"
        );
        assert!(output[0] > 0);
    }

    #[test]
    fn processed_sink_does_not_gate_quiet_incoming_signals() {
        let inner = Arc::new(QueuedAudioSink::new());
        let sink = ProcessedAudioSink::new(inner.clone(), 100.0, true, 1.0);
        sink.write(&[50, -50], VOICE_SAMPLE_RATE);
        let output = inner.take(2);
        assert_eq!(output, vec![50, -50]);
    }

    #[test]
    fn wav_sink_writes_a_finalized_pcm_header() {
        let path = std::env::temp_dir().join(format!("nektome-test-{}.wav", uuid::Uuid::new_v4()));
        {
            let sink = WavAudioSink::create(&path).unwrap();
            sink.write(&[1_000, -1_000], VOICE_SAMPLE_RATE);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wav_sink_resamples_opus_rate_audio_to_voice_rate() {
        let path =
            std::env::temp_dir().join(format!("nektome-opus-test-{}.wav", uuid::Uuid::new_v4()));
        {
            let sink = WavAudioSink::create(&path).unwrap();
            sink.write(&vec![1_000; 960], 48_000);
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 320);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn resampler_converts_16khz_to_8khz() {
        assert_eq!(resample_to_8khz(&[0, 100, 200, 300], 16_000).len(), 2);
    }

    #[test]
    fn resampler_drops_frames_for_invalid_device_rate() {
        assert!(resample_to_8khz(&[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn dynamics_gate_reduces_low_level_input() {
        let mut processor = AudioDynamicsProcessor::new(8_000);
        let mut pcm = vec![16; 80];
        processor.process(&mut pcm);
        assert!(pcm.iter().map(|sample| sample.abs()).max().unwrap() < 16);
    }

    #[test]
    fn microphone_processing_controls_can_bypass_dsp() {
        let mut processor = MicrophoneProcessor::new(8_000);
        processor.gain_percent = 100.0;
        processor.dsp_enabled = false;
        processor.warmth_enabled = false;
        processor.echo_cancellation_enabled = false;
        processor.noise_suppression_enabled = false;
        let mut pcm = vec![1_000; 80];
        processor.process(&mut pcm, 8_000);
        assert_eq!(pcm, vec![1_000; 80]);
    }

    #[test]
    fn vocal_warmth_saturates_peaks_and_preserves_low_levels() {
        let mut warmth = VocalWarmthProcessor::new();
        warmth.drive = 1.5;

        // The DC blocker high-passes constant signals; check that a small AC signal
        // (sign-alternating) is passed with minimal distortion (< 5% level change).
        let mut ac_quiet: Vec<i16> = (0..80)
            .map(|i| if i % 2 == 0 { 300 } else { -300 })
            .collect();
        let before_rms: f64 = ac_quiet.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        warmth.process(&mut ac_quiet);
        let after_rms: f64 = ac_quiet.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        // Small AC signal preserved within 10% (DC blocker high-passes
        // the 4 kHz alternating pattern by ~8%, which is expected).
        assert!(
            (after_rms - before_rms).abs() / before_rms < 0.10,
            "Small AC signal should pass near-unity; before_rms={before_rms}, after_rms={after_rms}"
        );

        let mut loud = vec![25_000i16; 80];
        warmth.reset();
        warmth.process(&mut loud);
        // Large signals are softly saturated/rounded
        assert!(loud[40] < 24_000, "loud[40]={}", loud[40]);
        assert!(loud[40] > 0, "loud[40]={}", loud[40]);
    }

    #[test]
    fn saturation_profiles_match_their_parameters() {
        for profile in SaturationProfile::ALL {
            if profile == SaturationProfile::Custom {
                continue;
            }
            let (d, a, b) = profile.params().unwrap();
            assert_eq!(SaturationProfile::from_params(d, a, b), profile);
            assert!(!profile.label().is_empty());
            assert!(!profile.description().is_empty());

            let mut warmth = VocalWarmthProcessor::with_profile(profile);
            assert_eq!(warmth.drive, d);
            assert_eq!(warmth.asymmetry, a);
            assert_eq!(warmth.bias, b);

            warmth.apply_profile(SaturationProfile::Custom);
            assert_eq!(warmth.drive, d); // untouched on custom

            assert_eq!(
                profile.to_character_profile().to_saturation_profile(),
                profile,
                "Profile {:?} must isomorphic map to VoiceCharacterProfile",
                profile
            );
        }
        assert_eq!(
            SaturationProfile::from_params(2.99, 1.01, 0.04),
            SaturationProfile::Custom
        );
    }

    #[test]
    fn microphone_noise_suppression_reduces_stationary_noise() {
        let mut processor = MicrophoneProcessor::new(48_000);
        processor.dsp_enabled = false;
        processor.echo_cancellation_enabled = false;
        processor.noise_suppression_enabled = true;
        // Generate pseudo-random white noise over 40 frames (400 ms)
        let mut frames = vec![0i16; 480 * 40];
        let mut rng: u32 = 123456789;
        for sample in &mut frames {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let val = ((rng >> 16) as i32) % 2000 - 1000;
            *sample = val as i16;
        }
        let original_noise = frames.clone();
        for chunk in frames.chunks_mut(480) {
            processor.process(chunk, 48_000);
        }
        // Verify that the output was modified (denoised) and did not overflow/NaN.
        let last_frame = &frames[frames.len() - 480..];
        let orig_last_frame = &original_noise[original_noise.len() - 480..];
        let orig_rms: f64 = orig_last_frame
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / 480.0;
        let denoised_rms: f64 = last_frame
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / 480.0;
        assert!(
            denoised_rms < orig_rms,
            "RNNoise should attenuate pseudo-random noise: orig_rms={orig_rms}, denoised_rms={denoised_rms}"
        );
    }

    #[test]
    fn microphone_gain_scales_without_clipping_samples() {
        let mut processor = MicrophoneProcessor::new(8_000);
        processor.gain_percent = 200.0;
        processor.dsp_enabled = false;
        processor.warmth_enabled = false;
        processor.echo_cancellation_enabled = false;
        processor.noise_suppression_enabled = false;
        let mut pcm = vec![20_000, -20_000];
        processor.process(&mut pcm, 8_000);
        assert_eq!(pcm, vec![31_130, -31_130]);
    }

    #[test]
    fn microphone_echo_ducking_runs_before_dynamics_and_gain() {
        note_speaker_activity(&[20_000; 80]);
        let mut processor = MicrophoneProcessor::new(8_000);
        processor.gain_percent = 100.0;
        processor.dsp_enabled = false;
        processor.warmth_enabled = false;
        processor.echo_cancellation_enabled = true;
        processor.noise_suppression_enabled = false;
        let mut pcm = vec![10_000; 80];
        processor.process(&mut pcm, 8_000);
        assert!(pcm[0] > pcm[79]);
        assert!((3_000..5_000).contains(&pcm[79]));
    }

    #[test]
    fn microphone_meter_reports_normalized_rms() {
        record_microphone_level(&[16_384, 16_384]);
        assert!((microphone_level() - 0.5).abs() < 0.001);
        record_microphone_level(&[]);
        assert_eq!(microphone_level(), 0.0);
    }

    #[test]
    fn speaker_mute_state_round_trips_without_a_lock() {
        set_speaker_muted(true);
        assert!(speaker_muted());
        set_speaker_muted(false);
        assert!(!speaker_muted());
    }

    #[test]
    fn linear_resampler_preserves_continuity_across_chunks() {
        let mut resampler = LinearResampler::new();
        let chunk1: Vec<i16> = (0..480).map(|i| (i * 20) as i16).collect();
        let chunk2: Vec<i16> = (480..960).map(|i| (i * 20) as i16).collect();

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        resampler.resample_into(&chunk1, 48_000, 96_000, &mut out1);
        resampler.resample_into(&chunk2, 48_000, 96_000, &mut out2);

        assert!(!out1.is_empty());
        assert!(!out2.is_empty());
        let boundary_step = (i32::from(out2[0]) - i32::from(*out1.last().unwrap())).abs();
        assert!(
            boundary_step <= 11,
            "Boundary step too large: {boundary_step}"
        );
    }

    #[test]
    fn linear_resampler_handles_rate_change_gracefully() {
        let mut resampler = LinearResampler::new();
        let mut out = Vec::new();
        resampler.resample_into(&[100, 200, 300, 400], 16_000, 8_000, &mut out);
        assert_eq!(out.len(), 2);
        resampler.resample_into(&[100, 200, 300, 400], 32_000, 8_000, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn voice_presence_boosts_speech_frequencies() {
        let mut filter = VoicePresenceFilter::new(48_000);
        let mut signal: Vec<i16> = (0..960)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 3200.0 * (i as f32) / 48_000.0;
                (phase.sin() * 5000.0).round() as i16
            })
            .collect();
        let original = signal.clone();

        // 0.0 dB boost: bypass/noop
        filter.set_boost_db(0.0, 48_000);
        filter.process(&mut signal);
        assert_eq!(signal, original);

        // 6.0 dB boost: gain at 3.2 kHz peak should noticeably increase peak amplitude
        filter.set_boost_db(6.0, 48_000);
        filter.process(&mut signal);
        let orig_max = original[480..].iter().map(|&s| s.abs()).max().unwrap();
        let boost_max = signal[480..].iter().map(|&s| s.abs()).max().unwrap();
        assert!(
            boost_max > orig_max,
            "Presence filter should boost 3.2kHz formant: orig={orig_max}, boosted={boost_max}"
        );
    }

    #[test]
    fn voice_proximity_adjusts_chest_frequencies() {
        let mut filter = VoiceProximityFilter::new(48_000);
        let mut signal: Vec<i16> = (0..960)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 200.0 * (i as f32) / 48_000.0;
                (phase.sin() * 5000.0).round() as i16
            })
            .collect();
        let original = signal.clone();

        // 0.0 dB: bypass
        filter.set_gain_db(0.0, 48_000);
        filter.process(&mut signal);
        assert_eq!(signal, original);

        // +3.0 dB boost
        filter.set_gain_db(3.0, 48_000);
        filter.process(&mut signal);
        let orig_max = original[480..].iter().map(|&s| s.abs()).max().unwrap();
        let boost_max = signal[480..].iter().map(|&s| s.abs()).max().unwrap();
        assert!(
            boost_max > orig_max,
            "Proximity filter should boost 200Hz: orig={orig_max}, boosted={boost_max}"
        );

        // -3.0 dB cut
        let mut cut_signal = original.clone();
        filter.set_gain_db(-3.0, 48_000);
        filter.process(&mut cut_signal);
        let cut_max = cut_signal[480..].iter().map(|&s| s.abs()).max().unwrap();
        assert!(
            cut_max < orig_max,
            "Proximity filter should cut 200Hz: orig={orig_max}, cut={cut_max}"
        );
    }

    #[test]
    fn voice_air_boosts_high_frequencies() {
        let mut filter = VoiceAirFilter::new(48_000);
        let mut signal: Vec<i16> = (0..960)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 10000.0 * (i as f32) / 48_000.0;
                (phase.sin() * 5000.0).round() as i16
            })
            .collect();
        let original = signal.clone();

        // 0.0 dB: bypass
        filter.set_boost_db(0.0, 48_000);
        filter.process(&mut signal);
        assert_eq!(signal, original);

        // +3.0 dB boost at 48 kHz
        filter.set_boost_db(3.0, 48_000);
        filter.process(&mut signal);
        let orig_max = original[480..].iter().map(|&s| s.abs()).max().unwrap();
        let boost_max = signal[480..].iter().map(|&s| s.abs()).max().unwrap();
        assert!(
            boost_max > orig_max,
            "Air filter should boost 10kHz sheen: orig={orig_max}, boosted={boost_max}"
        );

        // At 8 kHz, 10 kHz is above Nyquist so air filter should safely bypass
        let mut filter_low = VoiceAirFilter::new(8_000);
        filter_low.set_boost_db(3.0, 8_000);
        let mut low_signal = vec![1000, 2000, -1000, -2000];
        let original_low = low_signal.clone();
        filter_low.process(&mut low_signal);
        assert_eq!(low_signal, original_low);
    }

    #[test]
    fn voice_character_profiles_roundtrip() {
        for profile in VoiceCharacterProfile::ALL {
            if profile == VoiceCharacterProfile::Custom {
                continue;
            }
            let p = profile.params().unwrap();
            assert_eq!(
                VoiceCharacterProfile::from_params(
                    p.presence_db,
                    p.drive,
                    p.asymmetry,
                    p.proximity_db,
                    p.air_db,
                    p.compressor_ratio,
                ),
                profile,
                "Profile {:?} must round-trip from its params",
                profile
            );
            assert_eq!(
                profile.to_saturation_profile().to_character_profile(),
                profile,
                "Profile {:?} must isomorphic map to SaturationProfile",
                profile
            );
            assert!(!profile.label().is_empty());
            assert!(!profile.description().is_empty());
        }
    }
}
