use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Result;
use lokalbot_core::voice::{AudioTurn, VoiceSample, label_samples, label_turns};
use lokalbot_core::{LibraryPaths, Segment, SpeakerAttribution, TimingPrecision, Transcript};
use serde::{Deserialize, Serialize};

use crate::asr::Asr;
use crate::attribution::renumber_speakers;
use crate::language::validate_language;
use crate::models::{
    self, SpeechModelPaths, WHISPER_TURBO_MODEL, WHISPER_TURBO_TR_KHAN_MODEL, ZIPFORMER_OAYK_MODEL,
};
use crate::sanitize::sanitize_segment_text;
use crate::text::{PromptEcho, normalized_text};
use crate::transcriber::{
    PreparedMeeting, SpeechError, TranscribeOptions, check_for_echo_indices, effective_threads,
    prepare_meeting_pieces, prepare_samples_pieces,
};
use crate::whisper_runner::{ComputeDevice, DeviceProvenance, RuntimeLoadConfig};

pub const MULTI_STT_MAX_SPAN_SECONDS: f64 = 28.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSlotConfig {
    pub id: String,
    pub device: ComputeDevice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSttConfig {
    pub primary: ModelSlotConfig,
    pub tr_backup: ModelSlotConfig,
    pub fast_helper: ModelSlotConfig,
    pub cpu_threads: usize,
    pub auto_fallback_to_cpu: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whisper_server_bin: Option<PathBuf>,
}

impl Default for MultiSttConfig {
    fn default() -> Self {
        Self::bundled_default()
    }
}

impl MultiSttConfig {
    pub fn bundled_default() -> Self {
        Self {
            primary: ModelSlotConfig {
                id: WHISPER_TURBO_MODEL.to_string(),
                device: ComputeDevice::Igpu,
            },
            tr_backup: ModelSlotConfig {
                id: WHISPER_TURBO_TR_KHAN_MODEL.to_string(),
                device: ComputeDevice::Igpu,
            },
            fast_helper: ModelSlotConfig {
                id: ZIPFORMER_OAYK_MODEL.to_string(),
                device: ComputeDevice::Cpu,
            },
            cpu_threads: 6,
            auto_fallback_to_cpu: true,
            models_dir: None,
            whisper_server_bin: None,
        }
    }

    pub fn resolver(&self) -> SpeechModelPaths {
        SpeechModelPaths {
            explicit_models_dir: self.models_dir.clone(),
            exe_dir: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSttSegment {
    pub segment_id: usize,
    pub start: f64,
    pub end: f64,
    pub speaker: String,
    pub primary: String,
    pub tr_backup: String,
    pub fast_helper: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_precision: Option<TimingPrecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<SpeakerAttribution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSttProvenance {
    pub primary: DeviceProvenance,
    pub tr_backup: DeviceProvenance,
    pub fast_helper: DeviceProvenance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPassTelemetry {
    pub role: String,
    pub model_id: String,
    pub provenance: DeviceProvenance,
    pub elapsed_ms: u64,
    pub segments_decoded: usize,
}

#[derive(Clone, Debug, Default)]
pub struct MultiSttResult {
    pub transcript: Transcript,
    pub segments: Vec<MultiSttSegment>,
    pub provenance: Option<MultiSttProvenance>,
    pub model_runs: Vec<ModelPassTelemetry>,
    pub turns: Vec<AudioTurn>,
    pub samples: Vec<VoiceSample>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MultiSttProgress {
    DetectingSpeech,
    IdentifyingSpeakers,
    LoadingModel {
        model_id: String,
        pass: usize,
        total_passes: usize,
    },
    Transcribing {
        model_id: String,
        pass: usize,
        total_passes: usize,
        fraction: f32,
    },
}

pub trait PassDecoder: Send {
    fn provenance(&self) -> DeviceProvenance;
    fn decode_segment(&self, samples_16k: &[f32], language: Option<&str>) -> Result<String>;
}

pub trait PassLoader: Send + Sync {
    fn load_pass(
        &self,
        slot: &ModelSlotConfig,
        language: Option<&str>,
        cpu_threads: usize,
        auto_fallback_to_cpu: bool,
    ) -> Result<Box<dyn PassDecoder>>;
}

struct AsrPassDecoder(Asr);

impl PassDecoder for AsrPassDecoder {
    fn provenance(&self) -> DeviceProvenance {
        self.0.provenance()
    }

    fn decode_segment(&self, samples_16k: &[f32], language: Option<&str>) -> Result<String> {
        let decoded = self.0.decode(samples_16k, language, &[])?;
        Ok(normalized_text(&decoded.text))
    }
}

struct DefaultPassLoader {
    paths: LibraryPaths,
    resolver: SpeechModelPaths,
    whisper_server_bin: Option<PathBuf>,
}

impl PassLoader for DefaultPassLoader {
    fn load_pass(
        &self,
        slot: &ModelSlotConfig,
        language: Option<&str>,
        cpu_threads: usize,
        auto_fallback_to_cpu: bool,
    ) -> Result<Box<dyn PassDecoder>> {
        let model = models::find(&slot.id)
            .filter(|m| m.is_transcription())
            .ok_or_else(|| anyhow::anyhow!("Unknown transcription model “{}”", slot.id))?;
        if !models::is_installed_with(&self.paths, &self.resolver, model) {
            return Err(SpeechError::NotInstalled(model.display_name.to_string()).into());
        }
        let cfg = RuntimeLoadConfig {
            preferred_device: slot.device,
            auto_fallback_to_cpu,
            threads: effective_threads(cpu_threads),
            models_paths: self.resolver.clone(),
            whisper_server_bin: self.whisper_server_bin.clone(),
        };
        let asr = Asr::load_with_config(&self.paths, model, language, cfg)?;
        Ok(Box::new(AsrPassDecoder(asr)))
    }
}

pub struct MultiSttRunner {
    paths: LibraryPaths,
    config: MultiSttConfig,
    loader: Arc<dyn PassLoader>,
    cancelled: AtomicBool,
}

impl MultiSttRunner {
    pub fn new(paths: LibraryPaths, config: MultiSttConfig) -> Self {
        let resolver = config.resolver();
        let loader = Arc::new(DefaultPassLoader {
            paths: paths.clone(),
            resolver,
            whisper_server_bin: config.whisper_server_bin.clone(),
        });
        Self {
            paths,
            config,
            loader,
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn with_loader(
        paths: LibraryPaths,
        config: MultiSttConfig,
        loader: Arc<dyn PassLoader>,
    ) -> Self {
        Self {
            paths,
            config,
            loader,
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(SpeechError::Cancelled.into())
        } else {
            Ok(())
        }
    }

    fn validate_all_languages(&self, language: Option<&str>) -> Result<()> {
        for slot in [
            &self.config.primary,
            &self.config.tr_backup,
            &self.config.fast_helper,
        ] {
            let display = models::find(&slot.id).map_or(slot.id.as_str(), |m| m.display_name);
            validate_language(&slot.id, display, language)?;
        }
        Ok(())
    }

    fn engine_name(&self) -> String {
        format!(
            "asymmetric-multi-stt ({} + {} + {})",
            self.config.primary.id, self.config.tr_backup.id, self.config.fast_helper.id
        )
    }

    pub fn transcribe_meeting_multi(
        &self,
        mic: Option<&Path>,
        system: Option<&Path>,
        options: &TranscribeOptions,
        progress: &mut dyn FnMut(MultiSttProgress),
    ) -> Result<MultiSttResult> {
        self.cancelled.store(false, Ordering::Relaxed);
        let language = normalized_language(options.language.as_deref());
        self.validate_all_languages(language)?;
        let resolver = self.config.resolver();
        progress(MultiSttProgress::DetectingSpeech);
        let mut emitted_speakers = false;
        let prepared = prepare_meeting_pieces(
            &self.paths,
            &resolver,
            self.config.cpu_threads,
            mic,
            system,
            options,
            MULTI_STT_MAX_SPAN_SECONDS,
            &self.cancelled,
            &mut || {
                if !emitted_speakers {
                    emitted_speakers = true;
                    progress(MultiSttProgress::IdentifyingSpeakers);
                }
            },
        )?;
        self.execute_passes(prepared, mic, system, language, options, progress)
    }

    pub fn transcribe_samples_multi(
        &self,
        samples_16k: &[f32],
        options: &TranscribeOptions,
        progress: &mut dyn FnMut(MultiSttProgress),
    ) -> Result<MultiSttResult> {
        self.cancelled.store(false, Ordering::Relaxed);
        let language = normalized_language(options.language.as_deref());
        self.validate_all_languages(language)?;
        let resolver = self.config.resolver();
        progress(MultiSttProgress::DetectingSpeech);
        let prepared = prepare_samples_pieces(
            &self.paths,
            &resolver,
            self.config.cpu_threads,
            samples_16k,
            options,
            MULTI_STT_MAX_SPAN_SECONDS,
        )?;
        self.execute_passes(prepared, None, None, language, options, progress)
    }

    fn execute_passes(
        &self,
        prepared: PreparedMeeting,
        mic: Option<&Path>,
        system: Option<&Path>,
        language: Option<&str>,
        options: &TranscribeOptions,
        progress: &mut dyn FnMut(MultiSttProgress),
    ) -> Result<MultiSttResult> {
        if prepared.pieces.is_empty() {
            return Ok(MultiSttResult {
                transcript: Transcript {
                    segments: Vec::new(),
                    engine: self.engine_name(),
                    echo_report: prepared.echo_report,
                    ..Default::default()
                },
                ..Default::default()
            });
        }

        let (primary_texts, primary_telemetry) =
            self.run_pass("primary", &self.config.primary, 1, 3, &prepared.pieces, language, progress)?;
        let (backup_texts, backup_telemetry) =
            self.run_pass("tr_backup", &self.config.tr_backup, 2, 3, &prepared.pieces, language, progress)?;
        let (helper_texts, helper_telemetry) =
            self.run_pass("fast_helper", &self.config.fast_helper, 3, 3, &prepared.pieces, language, progress)?;

        let prompt_echo = |model_id: &str| -> Option<PromptEcho> {
            if models::accepts_vocabulary(model_id) {
                PromptEcho::new(options.vocabulary_prompt.as_deref())
            } else {
                None
            }
        };
        let p_echo = prompt_echo(&self.config.primary.id);
        let b_echo = prompt_echo(&self.config.tr_backup.id);
        let h_echo = prompt_echo(&self.config.fast_helper.id);

        let mut transcript_segments = Vec::with_capacity(prepared.pieces.len());
        let mut multi_segments = Vec::with_capacity(prepared.pieces.len());

        for (idx, piece) in prepared.pieces.iter().enumerate() {
            let duration = piece.end - piece.start;
            let p_clean = sanitize_segment_text(
                &primary_texts[idx],
                duration,
                Some(piece.precision),
                p_echo.as_ref(),
            )
            .unwrap_or_default();
            let b_clean = sanitize_segment_text(
                &backup_texts[idx],
                duration,
                Some(piece.precision),
                b_echo.as_ref(),
            )
            .unwrap_or_default();
            let h_clean = sanitize_segment_text(
                &helper_texts[idx],
                duration,
                Some(piece.precision),
                h_echo.as_ref(),
            )
            .unwrap_or_default();

            if p_clean.is_empty() && b_clean.is_empty() && h_clean.is_empty() {
                continue;
            }

            transcript_segments.push(Segment {
                start: piece.start,
                end: piece.end,
                speaker: piece.speaker.clone(),
                text: p_clean.clone(),
                confidence: None,
                timing_precision: Some(piece.precision),
                attribution: Some(piece.attribution),
            });
            multi_segments.push(MultiSttSegment {
                segment_id: multi_segments.len(),
                start: piece.start,
                end: piece.end,
                speaker: piece.speaker.clone(),
                primary: p_clean,
                tr_backup: b_clean,
                fast_helper: h_clean,
                timing_precision: Some(piece.precision),
                attribution: Some(piece.attribution),
            });
        }

        renumber_speakers(&mut transcript_segments);
        for (seg, multi) in transcript_segments.iter().zip(multi_segments.iter_mut()) {
            multi.speaker = seg.speaker.clone();
        }

        let kept_mask = check_for_echo_indices(
            &mut transcript_segments,
            mic,
            system,
            options,
            &self.cancelled,
        );
        let mut kept_multi = Vec::with_capacity(transcript_segments.len());
        for (idx, multi) in multi_segments.into_iter().enumerate() {
            if kept_mask.get(idx).copied().unwrap_or(false) {
                kept_multi.push(multi);
            }
        }
        for (new_id, (seg, multi)) in transcript_segments
            .iter()
            .zip(kept_multi.iter_mut())
            .enumerate()
        {
            multi.segment_id = new_id;
            multi.attribution = seg.attribution;
            multi.speaker = seg.speaker.clone();
        }

        let transcript = Transcript {
            segments: transcript_segments,
            engine: self.engine_name(),
            echo_report: prepared.echo_report,
            ..Default::default()
        };

        let mut turns = Vec::new();
        for (source, raw_turns) in &prepared.turns {
            turns.extend(label_turns(raw_turns, &transcript, *source));
        }
        let mut samples = Vec::new();
        for (source, raw_samples) in &prepared.samples {
            samples.extend(label_samples(raw_samples, &transcript, *source));
        }

        let provenance = MultiSttProvenance {
            primary: primary_telemetry.provenance.clone(),
            tr_backup: backup_telemetry.provenance.clone(),
            fast_helper: helper_telemetry.provenance.clone(),
        };

        Ok(MultiSttResult {
            transcript,
            segments: kept_multi,
            provenance: Some(provenance),
            model_runs: vec![primary_telemetry, backup_telemetry, helper_telemetry],
            turns,
            samples,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_pass(
        &self,
        role: &str,
        slot: &ModelSlotConfig,
        pass: usize,
        total_passes: usize,
        pieces: &[crate::transcriber::PreparedPiece],
        language: Option<&str>,
        progress: &mut dyn FnMut(MultiSttProgress),
    ) -> Result<(Vec<String>, ModelPassTelemetry)> {
        self.check_cancelled()?;
        progress(MultiSttProgress::LoadingModel {
            model_id: slot.id.clone(),
            pass,
            total_passes,
        });
        let started = Instant::now();
        let decoder = self.loader.load_pass(
            slot,
            language,
            self.config.cpu_threads,
            self.config.auto_fallback_to_cpu,
        )?;
        let provenance = decoder.provenance();
        let mut texts = Vec::with_capacity(pieces.len());
        let total_pieces = pieces.len().max(1);
        for (idx, piece) in pieces.iter().enumerate() {
            self.check_cancelled()?;
            let text = match decoder.decode_segment(&piece.samples, language) {
                Ok(text) => text,
                Err(error) if piece.end - piece.start < 1.0 => {
                    log::warn!(
                        "speech: {} skipped a {:.2}s span: {error:#}",
                        slot.id,
                        piece.end - piece.start
                    );
                    String::new()
                }
                Err(error) => return Err(error),
            };
            texts.push(text);
            progress(MultiSttProgress::Transcribing {
                model_id: slot.id.clone(),
                pass,
                total_passes,
                fraction: (idx + 1) as f32 / total_pieces as f32,
            });
        }
        drop(decoder);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        Ok((
            texts,
            ModelPassTelemetry {
                role: role.to_string(),
                model_id: slot.id.clone(),
                provenance,
                elapsed_ms,
                segments_decoded: pieces.len(),
            },
        ))
    }
}

fn normalized_language(language: Option<&str>) -> Option<&str> {
    language
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use parking_lot::Mutex;

    fn speech_with_pauses(bursts: usize) -> Vec<f32> {
        let rate = crate::vad::SAMPLE_RATE;
        let mut samples = Vec::new();
        for _ in 0..bursts {
            samples.extend((0..2 * rate).map(|i| {
                0.4 * (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate as f32).sin()
            }));
            samples.extend(std::iter::repeat_n(0.0, 3 * rate));
        }
        samples
    }

    struct MockPassDecoder {
        model_id: String,
        provenance: DeviceProvenance,
        replies: Vec<String>,
        call_index: AtomicUsize,
        active: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl PassDecoder for MockPassDecoder {
        fn provenance(&self) -> DeviceProvenance {
            self.provenance.clone()
        }

        fn decode_segment(&self, _samples_16k: &[f32], _language: Option<&str>) -> Result<String> {
            let idx = self.call_index.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .push(format!("decode:{}:{idx}", self.model_id));
            Ok(self
                .replies
                .get(idx)
                .cloned()
                .unwrap_or_else(|| self.replies.last().cloned().unwrap_or_default()))
        }
    }

    impl Drop for MockPassDecoder {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.events.lock().push(format!("unload:{}", self.model_id));
        }
    }

    struct MockPassLoader {
        replies_by_model: HashMap<String, Vec<String>>,
        simulate_igpu_failure: bool,
        active: Arc<AtomicUsize>,
        peak_active: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl PassLoader for MockPassLoader {
        fn load_pass(
            &self,
            slot: &ModelSlotConfig,
            _language: Option<&str>,
            _cpu_threads: usize,
            auto_fallback_to_cpu: bool,
        ) -> Result<Box<dyn PassDecoder>> {
            if self.simulate_igpu_failure
                && slot.device == ComputeDevice::Igpu
                && !auto_fallback_to_cpu
            {
                anyhow::bail!("iGPU unavailable and auto_fallback_to_cpu is false");
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_active.fetch_max(now, Ordering::SeqCst);
            self.events.lock().push(format!("load:{}", slot.id));
            let (actual_device, fallback_occurred, fallback_reason) =
                if self.simulate_igpu_failure && slot.device == ComputeDevice::Igpu {
                    (
                        ComputeDevice::Cpu,
                        true,
                        Some("simulated iGPU probe failure".to_string()),
                    )
                } else {
                    (slot.device, false, None)
                };
            Ok(Box::new(MockPassDecoder {
                model_id: slot.id.clone(),
                provenance: DeviceProvenance {
                    model_id: slot.id.clone(),
                    requested_device: slot.device,
                    actual_device,
                    fallback_occurred,
                    fallback_reason,
                    backend_detail: "mock".to_string(),
                },
                replies: self
                    .replies_by_model
                    .get(&slot.id)
                    .cloned()
                    .unwrap_or_default(),
                call_index: AtomicUsize::new(0),
                active: Arc::clone(&self.active),
                events: Arc::clone(&self.events),
            }))
        }
    }

    #[test]
    fn sequential_model_outer_loop_unloads_before_loading_next() {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(dir.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let peak_active = Arc::new(AtomicUsize::new(0));

        let mut replies = HashMap::new();
        replies.insert(
            WHISPER_TURBO_MODEL.to_string(),
            vec!["bir".into(), "iki".into()],
        );
        replies.insert(
            WHISPER_TURBO_TR_KHAN_MODEL.to_string(),
            vec!["bir tr".into(), "iki tr".into()],
        );
        replies.insert(
            ZIPFORMER_OAYK_MODEL.to_string(),
            vec!["bir zf".into(), "iki zf".into()],
        );

        let loader = Arc::new(MockPassLoader {
            replies_by_model: replies,
            simulate_igpu_failure: false,
            active: Arc::clone(&active),
            peak_active: Arc::clone(&peak_active),
            events: Arc::clone(&events),
        });
        let runner = MultiSttRunner::with_loader(paths, MultiSttConfig::bundled_default(), loader);
        let result = runner
            .transcribe_samples_multi(
                &speech_with_pauses(2),
                &TranscribeOptions {
                    language: Some("tr".into()),
                    ..Default::default()
                },
                &mut |_| {},
            )
            .unwrap();

        assert_eq!(peak_active.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            *events.lock(),
            vec![
                "load:whisper-turbo",
                "decode:whisper-turbo:0",
                "decode:whisper-turbo:1",
                "unload:whisper-turbo",
                "load:whisper-turbo-tr-khan",
                "decode:whisper-turbo-tr-khan:0",
                "decode:whisper-turbo-tr-khan:1",
                "unload:whisper-turbo-tr-khan",
                "load:zipformer-oayk",
                "decode:zipformer-oayk:0",
                "decode:zipformer-oayk:1",
                "unload:zipformer-oayk",
            ]
        );
        assert_eq!(result.segments.len(), 2);
        assert_eq!(result.model_runs.len(), 3);
    }

    #[test]
    fn no_rover_preserves_primary_backbone_when_helpers_disagree() {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(dir.path());
        let mut replies = HashMap::new();
        replies.insert(WHISPER_TURBO_MODEL.to_string(), vec!["A".into()]);
        replies.insert(WHISPER_TURBO_TR_KHAN_MODEL.to_string(), vec!["B".into()]);
        replies.insert(ZIPFORMER_OAYK_MODEL.to_string(), vec!["B".into()]);

        let loader = Arc::new(MockPassLoader {
            replies_by_model: replies,
            simulate_igpu_failure: false,
            active: Arc::new(AtomicUsize::new(0)),
            peak_active: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        let runner = MultiSttRunner::with_loader(paths, MultiSttConfig::bundled_default(), loader);
        let result = runner
            .transcribe_samples_multi(
                &speech_with_pauses(1),
                &TranscribeOptions::default(),
                &mut |_| {},
            )
            .unwrap();

        assert_eq!(result.transcript.segments.len(), 1);
        assert_eq!(result.transcript.segments[0].text, "A");
        assert_eq!(result.segments[0].primary, "A");
        assert_eq!(result.segments[0].tr_backup, "B");
        assert_eq!(result.segments[0].fast_helper, "B");
    }

    #[test]
    fn cpu_fallback_triggers_when_igpu_unavailable_and_fails_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(dir.path());
        let mut replies = HashMap::new();
        replies.insert(WHISPER_TURBO_MODEL.to_string(), vec!["merhaba".into()]);
        replies.insert(WHISPER_TURBO_TR_KHAN_MODEL.to_string(), vec!["merhaba".into()]);
        replies.insert(ZIPFORMER_OAYK_MODEL.to_string(), vec!["merhaba".into()]);

        let audio = speech_with_pauses(1);
        let loader_ok = Arc::new(MockPassLoader {
            replies_by_model: replies.clone(),
            simulate_igpu_failure: true,
            active: Arc::new(AtomicUsize::new(0)),
            peak_active: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        let runner_ok = MultiSttRunner::with_loader(
            paths.clone(),
            MultiSttConfig::bundled_default(),
            loader_ok,
        );
        let res = runner_ok
            .transcribe_samples_multi(&audio, &TranscribeOptions::default(), &mut |_| {})
            .unwrap();
        let prov = res.provenance.unwrap();
        assert_eq!(prov.primary.actual_device, ComputeDevice::Cpu);
        assert!(prov.primary.fallback_occurred);
        assert!(prov.primary.fallback_reason.is_some());

        let mut no_fb_cfg = MultiSttConfig::bundled_default();
        no_fb_cfg.auto_fallback_to_cpu = false;
        let loader_err = Arc::new(MockPassLoader {
            replies_by_model: replies,
            simulate_igpu_failure: true,
            active: Arc::new(AtomicUsize::new(0)),
            peak_active: Arc::new(AtomicUsize::new(0)),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        let runner_err = MultiSttRunner::with_loader(paths, no_fb_cfg, loader_err);
        assert!(
            runner_err
                .transcribe_samples_multi(&audio, &TranscribeOptions::default(), &mut |_| {})
                .is_err()
        );
    }
}
