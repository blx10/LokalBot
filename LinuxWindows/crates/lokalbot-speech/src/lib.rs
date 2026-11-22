mod asr;
mod attribution;
mod bleed_gate;
mod diarize;
mod echo;
mod echo_evidence;
pub mod kokoro;
pub mod language;
pub mod models;
pub mod multi_asr;
mod sanitize;
mod text;
mod transcriber;
mod vad;
mod voice;
pub mod whisper_runner;

pub use asr::qwen_language_name;
pub use attribution::renumber_speakers;
pub use attribution::{Region, Turn, apply_microphone_default, regions, track_attribution};
pub use bleed_gate::BleedGate;
pub use diarize::{CLUSTER_THRESHOLD, Diarizer};
pub use echo::{EchoCanceller, EchoRemoval, remove_echo};
pub use echo_evidence::EchoAnalysis;
pub use models::{
    SpeechModel, SpeechModelPaths, diarization_models, transcription_models, vad_model,
};
pub use multi_asr::{
    MULTI_STT_MAX_SPAN_SECONDS, ModelPassTelemetry, ModelSlotConfig, MultiSttConfig,
    MultiSttProgress, MultiSttProvenance, MultiSttResult, MultiSttRunner, MultiSttSegment,
    PassDecoder, PassLoader,
};
pub use sanitize::{SanitizeReport, sanitize};
pub use text::{PromptEcho, normalized_text, prompt_terms};
pub use transcriber::{
    DetailedTranscript, MIN_AUDIO_SECONDS, MIN_SPEECH_SECONDS, RawSegment, SpeechError,
    SpeechServer, TranscribeOptions, TranscribeProgress, Transcriber, analyze_call_voices,
};
pub use voice::{VoiceChunk, VoiceExtractor, voice_chunks, voice_samples};
pub use whisper_runner::{ComputeDevice, DeviceProvenance, RuntimeLoadConfig, WhisperRunner};
