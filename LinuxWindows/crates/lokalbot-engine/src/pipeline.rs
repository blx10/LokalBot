use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use parking_lot::{Condvar, Mutex};
use uuid::Uuid;

use lokalbot_ai::remote_gate::Priority;
use lokalbot_core::{Meeting, Transcript};

use crate::models::Models;
use crate::{BackgroundItem, BackgroundKind, EngineEvent, Shared, Stage};

#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    models: Models,
    queue: Mutex<VecDeque<Job>>,
    signal: Condvar,
    stop: AtomicBool,
    active: Mutex<Option<Uuid>>,
    waiting: Mutex<Vec<(Job, Need)>>,
}

const IDLE_WAKE: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};

#[derive(Clone, Debug)]
struct Job {
    meeting_id: Uuid,
    transcribe: bool,
    summarize: bool,
    summary_follows_setting: bool,
    user_initiated: bool,
    resumed: bool,
}

impl Job {
    fn resumed(saved: &lokalbot_core::PipelineJob) -> Self {
        Self {
            meeting_id: saved.meeting_id,
            transcribe: saved.transcribe && !saved.transcription_completed,
            summarize: saved.summarize,
            summary_follows_setting: saved.summary_follows_setting,
            user_initiated: false,
            resumed: true,
        }
    }

    fn writes_notes(&self, auto_summarize: bool) -> bool {
        self.summarize && (!self.summary_follows_setting || auto_summarize)
    }

    fn merge(&mut self, transcribe: bool, summarize: bool, user_initiated: bool) {
        self.transcribe |= transcribe;
        self.summarize = if user_initiated {
            summarize
        } else {
            self.summarize || summarize
        };
        self.summary_follows_setting &= !user_initiated;
        self.user_initiated |= user_initiated;
    }
}

enum Outcome {
    Done,
    WaitingForModels(Need),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Need {
    Transcription,
    Think,
}

impl Pipeline {
    pub fn start(shared: Arc<Shared>, models: Models) -> Self {
        let inner = Arc::new(Inner {
            shared: shared.clone(),
            models,
            queue: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            stop: AtomicBool::new(false),
            active: Mutex::new(None),
            waiting: Mutex::new(Vec::new()),
        });
        match shared.library.list_meetings().and_then(|meetings| {
            let existing: Vec<Uuid> = meetings.iter().map(|meeting| meeting.id).collect();
            shared.library.prune_jobs(&existing)
        }) {
            Ok(0) => {}
            Ok(removed) => log::info!("dropped {removed} processing job(s) for deleted meetings"),
            Err(err) => log::warn!("could not prune the processing queue: {err:#}"),
        }
        match shared.library.pending_jobs() {
            Ok(jobs) => {
                let mut queue = inner.queue.lock();
                for job in jobs {
                    shared.emit(EngineEvent::Processing {
                        meeting_id: job.meeting_id,
                        stage: Some(Stage::Queued),
                    });
                    queue.push_back(Job::resumed(&job));
                }
            }
            Err(err) => log::warn!("could not read pending jobs: {err:#}"),
        }
        match shared.library.parked_jobs() {
            Ok(jobs) => {
                for job in jobs {
                    let message = job
                        .last_error
                        .unwrap_or_else(|| "Processing stopped after repeated failures.".into());
                    shared.emit(EngineEvent::Processing {
                        meeting_id: job.meeting_id,
                        stage: Some(Stage::Failed(message)),
                    });
                }
            }
            Err(err) => log::warn!("could not read parked jobs: {err:#}"),
        }
        let worker = inner.clone();
        std::thread::Builder::new()
            .name("lokalbot-pipeline".into())
            .spawn(move || worker.run())
            .expect("spawning the pipeline thread");
        crate::speech::start_idle_release(shared.speech.clone(), shared.clone());
        let pipeline = Self { inner };
        let settings = shared.settings();
        if settings.auto_transcribe {
            let repair = pipeline.clone();
            std::thread::Builder::new()
                .name("lokalbot-transcript-repair".into())
                .spawn(move || {
                    repair.enqueue_missing_transcriptions(settings.auto_summarize);
                })
                .expect("spawning the transcript repair");
        }
        pipeline
    }

    pub fn enqueue_missing_transcriptions(&self, summarize: bool) -> Vec<Uuid> {
        let shared = &self.inner.shared;
        let library = &shared.library;
        if library.is_read_only() {
            return Vec::new();
        }
        let meetings = match library.list_meetings() {
            Ok(meetings) => meetings,
            Err(err) => {
                log::warn!("could not look for missing transcripts: {err:#}");
                return Vec::new();
            }
        };
        let mut queued = Vec::new();
        for meeting in meetings {
            if self.is_processing(meeting.id) || library.job(meeting.id).ok().flatten().is_some() {
                continue;
            }
            let Some(reason) = lokalbot_core::health::missing_transcript(library, &meeting) else {
                continue;
            };
            if reason == lokalbot_core::health::MissingTranscript::MergedGap
                && let Err(err) = lokalbot_core::health::mark_gap_repair_requested(
                    &library.meeting_dir(&meeting),
                    chrono::Utc::now(),
                )
            {
                log::warn!("transcript repair skipped for {}: {err:#}", meeting.id);
                continue;
            }
            if self.enqueue(meeting.id, true, summarize, false) {
                log::info!("queued missing transcript for {} ({reason:?})", meeting.id);
                queued.push(meeting.id);
            }
        }
        queued
    }

    pub fn transcription_ready(&self) -> bool {
        let models = &self.inner.models;
        models
            .missing(&models.meeting_requirements(false))
            .is_empty()
            && !(models.multi_stt_config().is_none() && models.transcription_server_blocked())
    }

    pub fn defer_until_next_launch(&self, meeting_id: Uuid, summarize: bool) -> Result<()> {
        let library = &self.inner.shared.library;
        if library.job(meeting_id)?.is_none() {
            library.enqueue_job(meeting_id, true, summarize, false)?;
        }
        Ok(())
    }

    pub fn enqueue(
        &self,
        meeting_id: Uuid,
        transcribe: bool,
        summarize: bool,
        user_initiated: bool,
    ) -> bool {
        let shared = &self.inner.shared;
        if let Err(err) =
            shared
                .library
                .enqueue_job(meeting_id, transcribe, summarize, user_initiated)
        {
            log::warn!("could not persist the processing job: {err:#}");
            shared.emit(EngineEvent::Processing {
                meeting_id,
                stage: Some(Stage::Failed(
                    "Could not persist the processing job.".into(),
                )),
            });
            return false;
        }
        let parked = {
            let mut waiting = self.inner.waiting.lock();
            waiting
                .iter()
                .position(|(job, _)| job.meeting_id == meeting_id)
                .map(|index| waiting.remove(index).0)
        };
        {
            let mut queue = self.inner.queue.lock();
            if let Some(existing) = queue.iter_mut().find(|job| job.meeting_id == meeting_id) {
                existing.merge(transcribe, summarize, user_initiated);
            } else if let Some(mut parked) = parked {
                parked.merge(transcribe, summarize, user_initiated);
                queue.push_back(parked);
            } else {
                queue.push_back(Job {
                    meeting_id,
                    transcribe,
                    summarize,
                    summary_follows_setting: !user_initiated,
                    user_initiated,
                    resumed: false,
                });
            }
        }
        shared.emit(EngineEvent::Processing {
            meeting_id,
            stage: Some(Stage::Queued),
        });
        self.inner.signal.notify_one();
        true
    }

    pub fn is_idle(&self) -> bool {
        self.inner.active.lock().is_none() && self.inner.queue.lock().is_empty()
    }

    pub fn is_processing(&self, meeting_id: Uuid) -> bool {
        *self.inner.active.lock() == Some(meeting_id)
            || self
                .inner
                .queue
                .lock()
                .iter()
                .any(|job| job.meeting_id == meeting_id)
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.signal.notify_all();
    }
}

impl Inner {
    fn run(self: Arc<Self>) {
        loop {
            let job = loop {
                if self.stop.load(Ordering::Relaxed) {
                    return;
                }
                self.requeue_ready();
                let mut queue = self.queue.lock();
                if let Some(job) = queue.pop_front() {
                    *self.active.lock() = Some(job.meeting_id);
                    break job;
                }
                self.signal.wait_for(&mut queue, IDLE_WAKE);
            };
            self.publish_queue_length();
            let meeting_id = job.meeting_id;
            self.shared.pipeline_busy.store(true, Ordering::Relaxed);
            match self.process(&job) {
                Ok(Outcome::Done) => {
                    let _ = self.shared.library.mark_job_completed(meeting_id);
                    self.shared.emit(EngineEvent::Processing {
                        meeting_id,
                        stage: None,
                    });
                }
                Ok(Outcome::WaitingForModels(need)) => {
                    self.park(job, need);
                    self.shared.emit(EngineEvent::Processing {
                        meeting_id,
                        stage: Some(Stage::WaitingForModels),
                    });
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    log::error!("processing {meeting_id} failed: {message}");
                    let _ = self.shared.library.mark_job_failed(meeting_id, &message);
                    self.shared.emit(EngineEvent::Processing {
                        meeting_id,
                        stage: Some(Stage::Failed(message)),
                    });
                }
            }
            *self.active.lock() = None;
            self.shared
                .clear_background(&format!("meeting:{meeting_id}"));
            let more = !self.queue.lock().is_empty();
            self.shared.pipeline_busy.store(more, Ordering::Relaxed);
            self.shared.exports.reconsider_notes();
            self.shared.routines.reconsider_evidence();
            self.shared.emit(EngineEvent::LibraryChanged);
        }
    }

    fn park(&self, mut job: Job, need: Need) {
        if need == Need::Think {
            job.transcribe = false;
        }
        self.waiting.lock().push((job, need));
    }

    fn ready(&self, need: Need) -> bool {
        match need {
            Need::Transcription => {
                self.models
                    .missing(&self.models.meeting_requirements(false))
                    .is_empty()
                    && !(self.models.multi_stt_config().is_none()
                        && self.models.transcription_server_blocked())
            }
            Need::Think => self
                .models
                .missing(&self.models.think_requirements())
                .is_empty(),
        }
    }

    fn requeue_ready(&self) {
        let needs: Vec<Need> = {
            let waiting = self.waiting.lock();
            if waiting.is_empty() {
                return;
            }
            [Need::Transcription, Need::Think]
                .into_iter()
                .filter(|need| waiting.iter().any(|(_, parked)| parked == need))
                .collect()
        };
        let ready: Vec<Need> = needs.into_iter().filter(|need| self.ready(*need)).collect();
        let auto_summarize = self.shared.settings().auto_summarize;
        let jobs: Vec<Job> = {
            let mut waiting = self.waiting.lock();
            let (ready, parked) =
                std::mem::take(&mut *waiting)
                    .into_iter()
                    .partition(|(job, need)| {
                        ready.contains(need)
                            || (*need == Need::Think && !job.writes_notes(auto_summarize))
                    });
            *waiting = parked;
            ready.into_iter().map(|(job, _)| job).collect()
        };
        if jobs.is_empty() {
            return;
        }
        log::info!(
            "pipeline rechecking {} job(s) that waited for models",
            jobs.len()
        );
        for job in jobs {
            let meeting_id = job.meeting_id;
            {
                let mut queue = self.queue.lock();
                if queue.iter().all(|queued| queued.meeting_id != meeting_id) {
                    queue.push_back(job);
                }
            }
            self.shared.emit(EngineEvent::Processing {
                meeting_id,
                stage: Some(Stage::Queued),
            });
        }
    }

    fn publish_queue_length(&self) {
        let queued = self.queue.lock().len();
        if queued == 0 {
            self.shared.clear_background("meeting-queue");
        }
    }

    fn stage(&self, meeting: &Meeting, stage: Stage, detail: &str, progress: Option<f32>) {
        let queued = self.queue.lock().len();
        let detail = if queued > 0 {
            format!("{detail} · {queued} queued")
        } else {
            detail.to_string()
        };
        self.shared.set_background(BackgroundItem {
            id: format!("meeting:{}", meeting.id),
            kind: BackgroundKind::Meeting,
            title: meeting.display_title(),
            detail,
            progress,
        });
        self.shared.emit(EngineEvent::Processing {
            meeting_id: meeting.id,
            stage: Some(stage),
        });
    }

    fn yield_to_capture(&self) {
        while self.shared.capture_active.load(Ordering::Relaxed)
            && !self.stop.load(Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn recover_speakers(&self, meeting: &Meeting, transcript: Transcript) -> Result<Transcript> {
        let library = &self.shared.library;
        let sealed = library
            .meeting_dir(meeting)
            .join(lokalbot_core::voice::EVIDENCE_FOLDER)
            .join(lokalbot_core::voice::IDENTITY_FILE);
        if !sealed.is_file() {
            return Ok(transcript);
        }
        let memory = crate::voice::memory(library);
        let recovered = memory.recover(meeting, &transcript).unwrap_or_else(|err| {
            log::info!(
                "speaker identity recovery skipped meeting={}: {err}",
                meeting.id
            );
            transcript
        });
        let recovered = memory.applying_latest_decision(&recovered, meeting.id);
        library.save_new_transcript(meeting, &recovered)?;
        Ok(recovered)
    }

    fn process(&self, job: &Job) -> Result<Outcome> {
        let library = &self.shared.library;
        let Some(meeting) = library.meeting(job.meeting_id)? else {
            return Ok(Outcome::Done);
        };
        if meeting.is_live() {
            return Ok(Outcome::Done);
        }
        self.yield_to_capture();
        let settings = self.shared.settings();

        let has_transcript = library.transcript(&meeting)?.is_some();
        if job.transcribe || !has_transcript {
            let multi_stt_cfg = self.models.multi_stt_config();
            let requirements = self.models.meeting_requirements(false);
            if !self.models.ensure(&requirements, job.user_initiated)? {
                return Ok(Outcome::WaitingForModels(Need::Transcription));
            }
            if !job.user_initiated
                && multi_stt_cfg.is_none()
                && self.models.transcription_server_blocked()
            {
                return Ok(Outcome::WaitingForModels(Need::Transcription));
            }
            library.mark_job_started(meeting.id)?;
            self.stage(
                &meeting,
                Stage::PreparingModel("speech model".into()),
                "Preparing transcription model",
                None,
            );
            let audio = library.audio(&meeting);
            if audio.mic.is_none() && audio.system.is_none() {
                anyhow::bail!("No recording available.");
            }
            let vocab_model_id = multi_stt_cfg
                .as_ref()
                .map(|c| c.primary.id.as_str())
                .unwrap_or(&settings.transcription_model);
            let options = lokalbot_speech::TranscribeOptions {
                language: Some(settings.transcription_language.clone())
                    .filter(|l| l != "auto" && !l.is_empty()),
                diarize: settings.multi_speaker_diarization,
                microphone_is_user: settings.microphone_is_user,
                vocabulary_prompt: Some(lokalbot_core::vocabulary::transcription_prompt(
                    &meeting,
                    library,
                    &settings,
                    lokalbot_speech::models::accepts_vocabulary(vocab_model_id),
                    job.resumed,
                ))
                .filter(|p| !p.trim().is_empty()),
                content_range: meeting.valid_content_range().map(|r| (r.start, r.end)),
                collect_voice_samples: !meeting.is_sample(),
                echo: settings
                    .echo_cancellation
                    .then(|| lokalbot_core::voice::reference_clock(&library.meeting_dir(&meeting))),
                clock: Some(lokalbot_core::voice::reference_clock(
                    &library.meeting_dir(&meeting),
                )),
            };
            self.stage(&meeting, Stage::Transcribing, "Transcribing", None);
            let (transcript, multi_segments) = if let Some(multi_cfg) = multi_stt_cfg.as_ref() {
                let (transcript, multi_result) = transcribe_meeting_multi_stt(
                    &self.shared,
                    &meeting,
                    multi_cfg,
                    audio.mic.as_deref(),
                    audio.system.as_deref(),
                    &options,
                    &mut |progress| match progress {
                        lokalbot_speech::MultiSttProgress::DetectingSpeech => self.stage(
                            &meeting,
                            Stage::PreparingModel("speech detection".into()),
                            "Preparing transcription model",
                            None,
                        ),
                        lokalbot_speech::MultiSttProgress::IdentifyingSpeakers => self.stage(
                            &meeting,
                            Stage::IdentifyingSpeakers,
                            "Separating speakers",
                            None,
                        ),
                        lokalbot_speech::MultiSttProgress::LoadingModel { model_id, .. } => self
                            .stage(
                                &meeting,
                                Stage::PreparingModel(model_id),
                                "Preparing transcription model",
                                None,
                            ),
                        lokalbot_speech::MultiSttProgress::Transcribing { fraction, .. } => {
                            self.shared.speech.stand_aside();
                            self.stage(
                                &meeting,
                                Stage::Transcribing,
                                "Transcribing",
                                Some(fraction),
                            )
                        }
                    },
                )?;
                (transcript, Some(multi_result.segments))
            } else {
                let detailed = self
                    .shared
                    .speech
                    .with_in_background(
                        &self.shared,
                        &settings.transcription_model,
                        |transcriber| {
                            transcriber.transcribe_meeting_detailed(
                                audio.mic.as_deref(),
                                audio.system.as_deref(),
                                &options,
                                &mut |progress| match progress {
                                    lokalbot_speech::TranscribeProgress::Transcribing {
                                        fraction,
                                    } => {
                                        self.shared.speech.stand_aside();
                                        self.stage(
                                            &meeting,
                                            Stage::Transcribing,
                                            "Transcribing",
                                            Some(fraction),
                                        )
                                    }
                                    lokalbot_speech::TranscribeProgress::IdentifyingSpeakers => {
                                        self.stage(
                                            &meeting,
                                            Stage::IdentifyingSpeakers,
                                            "Separating speakers",
                                            None,
                                        )
                                    }
                                    _ => {}
                                },
                            )
                        },
                    )
                    .context("transcription")?;
                let mut transcript = detailed.transcript;
                if let Some(range) = meeting.valid_content_range() {
                    range.apply(&mut transcript);
                }
                let previous = library.transcript(&meeting)?;
                let memory = crate::voice::memory(library);
                let evidence = crate::voice::microphone_evidence(
                    library,
                    &meeting,
                    &transcript,
                    &detailed.turns,
                    &detailed.samples,
                );
                let identified = memory.process(lokalbot_core::voice::ProcessRequest {
                    transcript: &transcript,
                    meeting: &meeting,
                    turns: &detailed.turns,
                    evidence,
                    mic: audio.mic.as_deref(),
                    system: audio.system.as_deref(),
                    previous: previous.as_ref(),
                    name_automatically: crate::voice::NAME_SPEAKERS_AUTOMATICALLY,
                });
                transcript = match identified {
                    Ok(named) => named,
                    Err(err) => {
                        log::warn!("speaker identity skipped meeting={}: {err}", meeting.id);
                        memory.note_unavailable(meeting.id, &err);
                        if let Some(previous) = &previous {
                            transcript.carry_speaker_names_from(previous);
                        }
                        transcript
                    }
                };
                let transcript = finished_transcript(
                    &meeting,
                    &library.meeting_dir(&meeting),
                    previous.as_ref(),
                    transcript,
                    options.vocabulary_prompt.as_deref(),
                )?;
                (
                    memory.applying_latest_decision(&transcript, meeting.id),
                    None,
                )
            };
            let transcript = self.post_correct_transcript(
                &meeting,
                &settings,
                transcript,
                multi_segments.as_deref(),
            )?;
            library.save_new_transcript(&meeting, &transcript)?;
            library.mark_transcription_completed(meeting.id)?;
            library.reindex_meeting(&meeting)?;
            self.shared.emit(EngineEvent::LibraryChanged);
            if transcript.segments.is_empty() {
                return Ok(Outcome::Done);
            }
        }

        if job.writes_notes(self.shared.settings().auto_summarize) {
            let Some(mut transcript) = library.transcript(&meeting)? else {
                return Ok(Outcome::Done);
            };
            let saved_prompt = lokalbot_core::vocabulary::saved_prompt(
                &settings.transcription_prompt,
                &library.meeting_dir(&meeting),
            );
            if lokalbot_speech::sanitize(&mut transcript, Some(&saved_prompt)).changed() {
                library.save_transcript(&meeting, &transcript)?;
            }
            if transcript.segments.is_empty() {
                return Ok(Outcome::Done);
            }
            let transcript = self.recover_speakers(&meeting, transcript)?;
            let requirements = self.models.think_requirements();
            if !self.models.ensure(&requirements, job.user_initiated)? {
                return Ok(Outcome::WaitingForModels(Need::Think));
            }
            library.mark_job_started(meeting.id)?;
            self.yield_to_capture();
            self.stage(
                &meeting,
                Stage::PreparingModel("summary model".into()),
                "Preparing Think model",
                None,
            );
            let think = self
                .shared
                .think(&settings)?
                .with_priority(Priority::Pipeline, "meeting notes");
            self.stage(&meeting, Stage::Summarizing, "Writing notes", None);
            let user_notes = library.notes(&meeting)?;
            let started = std::time::Instant::now();
            let folder = library.meeting_dir(&meeting);
            let options = lokalbot_ai::notes::NotesOptions {
                checkpoint_dir: Some(folder.clone()),
                retrying_stopped_parts: job.user_initiated,
                cancel: None,
                screen_titles: if settings.meeting_notes_use_screen_titles && !meeting.is_imported()
                {
                    library.meeting_screen_context(&meeting).material_titles()
                } else {
                    Vec::new()
                },
            };
            let notes = lokalbot_ai::notes::generate_meeting_notes_with(
                &think,
                &meeting,
                &transcript,
                &user_notes,
                &settings,
                &options,
                &mut |detail: &str| self.stage(&meeting, Stage::Summarizing, detail, None),
            )?;
            self.shared.generation_metrics.record(
                format!("Summary · {}", think.display_name()),
                started.elapsed().as_secs_f64(),
                &notes.summary_markdown,
            );
            if library.transcript(&meeting)?.as_ref() != Some(&transcript) {
                anyhow::bail!(
                    "Speaker attribution changed during generation. Refresh the meeting notes."
                );
            }
            let previous = match library.outcomes(&meeting)? {
                Some(outcomes) => Some(outcomes),
                None => library.previous_outcomes(&meeting)?,
            };
            let state = library.outcome_state(&meeting)?;
            library.save_outcomes(&meeting, &notes.outcomes)?;
            if let Some(previous) = &previous {
                let reconciled = lokalbot_core::meeting_review::reconcile_state(
                    &state,
                    previous,
                    &notes.outcomes,
                );
                library.save_outcome_state(&meeting, &reconciled)?;
            }
            let authoritative = library
                .outcome_projection(&meeting)?
                .map(|projection| projection.corrected_outcomes())
                .unwrap_or_else(|| notes.outcomes.clone());
            let summary = lokalbot_ai::notes::render::synchronize(
                &notes.summary_markdown,
                &authoritative,
                settings.note_template,
            );
            lokalbot_core::library::write_json(&folder.join("summary-claims.json"), &notes.claims)?;
            library.save_summary(&meeting, &summary)?;
            lokalbot_ai::notes::remove_checkpoint(&folder);
            let _ = std::fs::remove_file(folder.join(lokalbot_core::library::REFRESH_MARKER));
            library.reindex_meeting(&meeting)?;
            if settings.semantic_search_enabled
                && !crate::post_correction::local_llama_server_disabled(&settings)
                && self
                    .models
                    .missing(&self.models.embedding_requirements())
                    .is_empty()
                && let Err(err) =
                    lokalbot_ai::embeddings::index_meeting(&self.shared.runtime, library, &meeting)
            {
                log::warn!("updating meaning search failed: {err:#}");
            }
        }
        Ok(Outcome::Done)
    }

    fn post_correct_transcript(
        &self,
        meeting: &Meeting,
        settings: &lokalbot_core::Settings,
        transcript: lokalbot_core::Transcript,
        multi_segments: Option<&[lokalbot_speech::MultiSttSegment]>,
    ) -> Result<lokalbot_core::Transcript> {
        let segments = match multi_segments {
            Some(segs) if segs.len() == transcript.segments.len() => {
                crate::post_correction::segments_from_multi_stt(segs)
            }
            _ => crate::post_correction::segments_from_transcript(&transcript),
        };
        let (corrected, _) = crate::post_correction::correct_transcript(
            meeting,
            &self.shared.library,
            settings,
            transcript,
            &segments,
            &mut || {
                self.stage(meeting, Stage::PostCorrecting, "Refining transcript", None);
            },
        )?;
        Ok(corrected)
    }
}

pub(crate) fn transcribe_meeting_multi_stt(
    shared: &Shared,
    meeting: &Meeting,
    config: &crate::speech::MultiSttConfig,
    mic: Option<&std::path::Path>,
    system: Option<&std::path::Path>,
    options: &lokalbot_speech::TranscribeOptions,
    progress: &mut dyn FnMut(lokalbot_speech::MultiSttProgress),
) -> Result<(Transcript, lokalbot_speech::MultiSttResult)> {
    let library = &shared.library;
    let mut detailed = shared
        .speech
        .run_multi_stt_with_shared(shared, config, mic, system, options, progress)
        .context("multi-stt transcription")?;
    let mut transcript = detailed.transcript.clone();
    if let Some(range) = meeting.valid_content_range() {
        range.apply(&mut transcript);
        detailed
            .segments
            .retain(|s| s.start >= range.start && s.end <= range.end);
    }
    let previous = library.transcript(meeting)?;
    let memory = crate::voice::memory(library);
    let evidence = crate::voice::microphone_evidence(
        library,
        meeting,
        &transcript,
        &detailed.turns,
        &detailed.samples,
    );
    let identified = memory.process(lokalbot_core::voice::ProcessRequest {
        transcript: &transcript,
        meeting,
        turns: &detailed.turns,
        evidence,
        mic,
        system,
        previous: previous.as_ref(),
        name_automatically: crate::voice::NAME_SPEAKERS_AUTOMATICALLY,
    });
    transcript = match identified {
        Ok(named) => named,
        Err(err) => {
            log::warn!("speaker identity skipped meeting={}: {err}", meeting.id);
            memory.note_unavailable(meeting.id, &err);
            if let Some(previous) = &previous {
                transcript.carry_speaker_names_from(previous);
            }
            transcript
        }
    };
    let vocab_prompt = lokalbot_speech::models::accepts_vocabulary(&config.primary.id)
        .then_some(options.vocabulary_prompt.as_deref())
        .flatten();
    let transcript = finished_transcript(
        meeting,
        &library.meeting_dir(meeting),
        previous.as_ref(),
        transcript,
        vocab_prompt,
    )?;
    let transcript = memory.applying_latest_decision(&transcript, meeting.id);
    detailed.segments = reconcile_multi_segments(&transcript, detailed.segments);
    detailed.transcript = transcript.clone();
    Ok((transcript, detailed))
}

fn reconcile_multi_segments(
    transcript: &Transcript,
    previous_segments: Vec<lokalbot_speech::MultiSttSegment>,
) -> Vec<lokalbot_speech::MultiSttSegment> {
    let mut pool: Vec<Option<lokalbot_speech::MultiSttSegment>> =
        previous_segments.into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(transcript.segments.len());
    for (idx, seg) in transcript.segments.iter().enumerate() {
        let matched = pool.iter_mut().find_map(|slot| {
            let candidate = slot.as_ref()?;
            if (candidate.start - seg.start).abs() < 1e-6
                && (candidate.end - seg.end).abs() < 1e-6
                && candidate.primary == seg.text
            {
                slot.take()
            } else {
                None
            }
        });
        let mut entry = matched.unwrap_or_else(|| lokalbot_speech::MultiSttSegment {
            segment_id: idx,
            start: seg.start,
            end: seg.end,
            speaker: seg.speaker.clone(),
            primary: seg.text.clone(),
            tr_backup: String::new(),
            fast_helper: String::new(),
            timing_precision: seg.timing_precision,
            attribution: seg.attribution,
        });
        entry.segment_id = idx;
        entry.speaker = seg.speaker.clone();
        entry.attribution = seg.attribution;
        out.push(entry);
    }
    out
}

fn finished_transcript(
    meeting: &Meeting,
    folder: &std::path::Path,
    previous: Option<&lokalbot_core::Transcript>,
    regenerated: lokalbot_core::Transcript,
    prompt: Option<&str>,
) -> Result<lokalbot_core::Transcript> {
    let mut transcript = if meeting.is_merged_meeting() {
        lokalbot_core::merge::preserve_transcript_only_sources(previous, folder, regenerated)?
    } else {
        regenerated
    };
    let report = lokalbot_speech::sanitize(&mut transcript, prompt);
    if report.changed() {
        log::info!(
            "transcript cleanup after merge changed={} removed={} removed_words={}",
            report.changed_segments,
            report.removed_segments,
            report.removed_words
        );
    }
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished_recording(library: &lokalbot_core::Library, title: &str) -> Meeting {
        let started = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut meeting = library.create_meeting(title, "Zoom", started).unwrap();
        lokalbot_audio::write_wav_16k(
            &library.meeting_dir(&meeting).join(lokalbot_audio::MIC_FILE),
            &vec![0.25f32; 16_000 * 6],
        )
        .unwrap();
        meeting.ended_at = Some(started + chrono::Duration::minutes(5));
        meeting.recorded_duration = Some(6.0);
        library.save_meeting(&meeting).unwrap();
        meeting
    }

    fn library() -> (tempfile::TempDir, lokalbot_core::Library) {
        let dir = tempfile::tempdir().unwrap();
        let library =
            lokalbot_core::Library::open(lokalbot_core::LibraryPaths::new(dir.path())).unwrap();
        (dir, library)
    }

    fn pipeline(library: lokalbot_core::Library, settings: lokalbot_core::Settings) -> Pipeline {
        let shared = Shared::for_tests(library, settings, |_| {});
        let models = crate::models::Models::new(shared.clone());
        Pipeline::start(shared, models)
    }

    fn wait_for_jobs(library: &lokalbot_core::Library, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while library.pending_jobs().unwrap().len() < count {
            assert!(std::time::Instant::now() < deadline, "no job was queued");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn launch_queues_finished_recordings_that_never_got_a_transcript() {
        let (_dir, library) = library();
        let missing = finished_recording(&library, "Never transcribed");
        let settings = lokalbot_core::Settings {
            auto_transcribe: true,
            auto_summarize: false,
            ..Default::default()
        };
        let pipeline = pipeline(library.clone(), settings);
        pipeline.stop();
        wait_for_jobs(&library, 1);
        let job = library.job(missing.id).unwrap().unwrap();
        assert!(job.transcribe && !job.summarize);
        assert!(job.summary_follows_setting);
    }

    #[test]
    fn launch_leaves_recordings_alone_while_automatic_transcription_is_off() {
        let (_dir, library) = library();
        finished_recording(&library, "Kept as audio");
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            ..Default::default()
        };
        let pipeline = pipeline(library.clone(), settings);
        pipeline.stop();
        std::thread::sleep(Duration::from_millis(200));
        assert!(library.pending_jobs().unwrap().is_empty());
    }

    #[test]
    fn a_repair_skips_parked_and_transcribed_meetings() {
        let (_dir, library) = library();
        let parked = finished_recording(&library, "Failed three times");
        library.enqueue_job(parked.id, true, true, false).unwrap();
        for _ in 0..lokalbot_core::jobs::MAX_AUTO_RESUME_ATTEMPTS {
            library.mark_job_started(parked.id).unwrap();
        }
        let done = finished_recording(&library, "Transcribed");
        std::fs::write(library.meeting_dir(&done).join("transcript.json"), "{}").unwrap();
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            ..Default::default()
        };
        let pipeline = pipeline(library.clone(), settings);
        pipeline.stop();
        assert!(pipeline.enqueue_missing_transcriptions(false).is_empty());
        assert!(library.pending_jobs().unwrap().is_empty());
    }

    #[test]
    fn a_merged_gap_is_repaired_at_most_once() {
        let (_dir, library) = library();
        let mut meeting = finished_recording(&library, "Merged");
        meeting.recorded_duration = Some(200.0);
        meeting.merged_source_meeting_ids = Some(vec![Uuid::new_v4(), Uuid::new_v4()]);
        library.save_meeting(&meeting).unwrap();
        let folder = library.meeting_dir(&meeting);
        let started = meeting.started_at;
        let source = |duration: f64| lokalbot_core::merge::ManifestSource {
            id: Uuid::new_v4(),
            title: "Source".into(),
            started_at: started,
            duration,
            has_audio: Some(true),
            has_transcript: Some(true),
        };
        std::fs::write(
            folder.join(lokalbot_core::merge::MANIFEST_FILE),
            serde_json::to_vec(&lokalbot_core::merge::Manifest {
                version: 2,
                sources: vec![source(100.0), source(100.0)],
            })
            .unwrap(),
        )
        .unwrap();
        let transcript = lokalbot_core::Transcript {
            segments: vec![lokalbot_core::Segment {
                start: 1.0,
                end: 5.0,
                speaker: "source1:them".into(),
                text: "Only the first half.".into(),
                confidence: None,
                timing_precision: None,
                attribution: None,
            }],
            ..Default::default()
        };
        library.save_new_transcript(&meeting, &transcript).unwrap();
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            ..Default::default()
        };
        let pipeline = pipeline(library.clone(), settings);
        pipeline.stop();

        assert_eq!(
            pipeline.enqueue_missing_transcriptions(true),
            vec![meeting.id]
        );
        let marker: serde_json::Value = serde_json::from_slice(
            &std::fs::read(folder.join("transcript-gap-repair.json")).unwrap(),
        )
        .unwrap();
        assert!(marker["requestedAt"].as_str().unwrap().ends_with('Z'));

        library.mark_job_completed(meeting.id).unwrap();
        assert!(pipeline.enqueue_missing_transcriptions(true).is_empty());
    }

    #[test]
    fn jobs_for_deleted_meetings_go_at_launch() {
        let (_dir, library) = library();
        let kept = finished_recording(&library, "Kept");
        library.enqueue_job(kept.id, true, false, false).unwrap();
        let gone = Uuid::new_v4();
        library.enqueue_job(gone, true, false, false).unwrap();
        let pipeline = pipeline(library.clone(), lokalbot_core::Settings::default());
        pipeline.stop();
        assert!(library.job(gone).unwrap().is_none());
        assert!(library.job(kept.id).unwrap().is_some());
    }

    fn merged_with_transcript_only_source(folder: &std::path::Path) -> Meeting {
        let started = chrono::Utc::now();
        let mut meeting = Meeting::new("Both", "Merged meetings", started, "both");
        meeting.merged_source_meeting_ids = Some(vec![Uuid::new_v4(), Uuid::new_v4()]);
        let source = |has_audio, has_transcript| lokalbot_core::merge::ManifestSource {
            id: Uuid::new_v4(),
            title: "Source".into(),
            started_at: started,
            duration: 100.0,
            has_audio: Some(has_audio),
            has_transcript: Some(has_transcript),
        };
        let manifest = lokalbot_core::merge::Manifest {
            version: 2,
            sources: vec![source(false, true), source(true, true)],
        };
        std::fs::write(
            folder.join(lokalbot_core::merge::MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        meeting
    }

    fn row(start: f64, end: f64, text: &str) -> lokalbot_core::Segment {
        lokalbot_core::Segment {
            start,
            end,
            speaker: "source1:them".into(),
            text: text.into(),
            confidence: None,
            timing_precision: None,
            attribution: None,
        }
    }

    #[test]
    fn rows_kept_from_a_transcript_only_source_are_cleaned_too() {
        let dir = tempfile::tempdir().unwrap();
        let meeting = merged_with_transcript_only_source(dir.path());
        let previous = lokalbot_core::Transcript {
            segments: vec![row(3.0, 13.0, &vec!["thank you"; 40].join(" "))],
            ..Default::default()
        };
        let regenerated = lokalbot_core::Transcript {
            segments: vec![row(120.0, 125.0, "Recorded speech")],
            ..Default::default()
        };
        let finished =
            finished_transcript(&meeting, dir.path(), Some(&previous), regenerated, None).unwrap();
        let texts: Vec<_> = finished.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["thank you thank you", "Recorded speech"]);
    }

    #[test]
    fn rows_that_only_repeat_the_vocabulary_are_removed_after_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let meeting = merged_with_transcript_only_source(dir.path());
        let prompt = "Vocabulary: LokalBot, Zagreb, Qwen, Parakeet.";
        let previous = lokalbot_core::Transcript {
            segments: vec![
                row(3.0, 13.0, prompt),
                row(20.0, 25.0, "Notes from the first call"),
            ],
            ..Default::default()
        };
        let finished = finished_transcript(
            &meeting,
            dir.path(),
            Some(&previous),
            lokalbot_core::Transcript::default(),
            Some(prompt),
        )
        .unwrap();
        let texts: Vec<_> = finished.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Notes from the first call"]);
    }

    fn saved(summary_follows_setting: bool) -> lokalbot_core::PipelineJob {
        lokalbot_core::PipelineJob {
            meeting_id: Uuid::new_v4(),
            transcribe: true,
            transcription_completed: true,
            summarize: true,
            summary_follows_setting,
            attempts: 1,
            last_error: None,
        }
    }

    #[test]
    fn a_resumed_automatic_summary_follows_the_current_setting() {
        let automatic = Job::resumed(&saved(true));
        assert!(
            !automatic.transcribe,
            "its transcript was already committed"
        );
        assert!(automatic.writes_notes(true));
        assert!(!automatic.writes_notes(false));

        let explicit = Job::resumed(&saved(false));
        assert!(
            explicit.writes_notes(false),
            "an explicit request still runs"
        );
    }

    #[test]
    fn an_explicit_request_overrides_a_queued_automatic_one() {
        let mut job = Job::resumed(&saved(true));
        job.merge(false, true, true);
        assert!(job.writes_notes(false));
        let mut job = Job::resumed(&saved(false));
        job.merge(true, true, false);
        assert!(
            job.writes_notes(false),
            "an automatic enqueue keeps it explicit"
        );
    }

    fn finished_without_audio(library: &lokalbot_core::Library) -> Meeting {
        let started = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut meeting = library
            .create_meeting("Recorded before the download", "Zoom", started)
            .unwrap();
        meeting.ended_at = Some(started + chrono::Duration::minutes(5));
        library.save_meeting(&meeting).unwrap();
        meeting
    }

    fn pretend_installed(library: &lokalbot_core::Library, id: &str) {
        let model = lokalbot_speech::models::find(id).unwrap();
        for file in model.files {
            let path = lokalbot_speech::models::model_file(library.paths(), model, file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"not a model").unwrap();
        }
        lokalbot_speech::models::finish_install(library.paths(), model).unwrap();
    }

    type Stages = Arc<Mutex<Vec<Option<Stage>>>>;

    fn wait_for_stage(stages: &Stages, wanted: impl Fn(&Option<Stage>) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !stages.lock().iter().any(&wanted) {
            assert!(
                std::time::Instant::now() < deadline,
                "stages so far: {:?}",
                stages.lock()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_recording_that_waited_for_models_is_processed_once_they_are_installed() {
        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            auto_summarize: false,
            ..Default::default()
        };
        let stages = Stages::default();
        let seen = stages.clone();
        let shared = Shared::for_tests(library.clone(), settings, move |event| {
            if let EngineEvent::Processing { stage, .. } = event {
                seen.lock().push(stage);
            }
        });
        let models = crate::models::Models::new(shared.clone());
        let needed = models.meeting_requirements(false);
        assert!(!needed.is_empty());
        let pipeline = Pipeline::start(shared, models);

        assert!(pipeline.enqueue(meeting.id, true, false, false));
        wait_for_stage(&stages, |stage| *stage == Some(Stage::WaitingForModels));
        std::thread::sleep(IDLE_WAKE * 4);
        assert_eq!(
            *stages.lock(),
            [Some(Stage::Queued), Some(Stage::WaitingForModels)]
        );
        assert_eq!(library.job(meeting.id).unwrap().unwrap().attempts, 0);

        for id in &needed {
            pretend_installed(&library, id);
        }
        wait_for_stage(&stages, |stage| matches!(stage, Some(Stage::Failed(_))));
        pipeline.stop();
        let stages = stages.lock();
        assert_eq!(
            stages[..3],
            [
                Some(Stage::Queued),
                Some(Stage::WaitingForModels),
                Some(Stage::Queued)
            ]
        );
        assert_eq!(
            stages.last().unwrap(),
            &Some(Stage::Failed("No recording available.".into()))
        );
        assert_eq!(library.job(meeting.id).unwrap().unwrap().attempts, 1);
    }

    fn stopped(settings: lokalbot_core::Settings) -> (tempfile::TempDir, Pipeline, Meeting) {
        let (dir, library) = library();
        let meeting = finished_without_audio(&library);
        let pipeline = pipeline(library, settings);
        pipeline.stop();
        std::thread::sleep(IDLE_WAKE * 4);
        (dir, pipeline, meeting)
    }

    fn transcribed(meeting: &Meeting) -> Job {
        Job {
            meeting_id: meeting.id,
            transcribe: true,
            summarize: true,
            summary_follows_setting: true,
            user_initiated: false,
            resumed: false,
        }
    }

    #[test]
    fn a_job_that_waits_for_its_notes_never_transcribes_again() {
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            summarizer_backend: lokalbot_core::settings::SummarizerBackend::Ollama,
            ..Default::default()
        };
        let (_dir, pipeline, meeting) = stopped(settings);
        let inner = &pipeline.inner;

        inner.park(transcribed(&meeting), Need::Think);
        inner.requeue_ready();
        {
            let mut queue = inner.queue.lock();
            assert_eq!(queue.len(), 1, "Think is ready, so the job goes on");
            assert!(!queue[0].transcribe, "it would transcribe a second time");
            assert!(queue[0].summarize);
            queue.clear();
        }

        inner.park(transcribed(&meeting), Need::Think);
        assert!(pipeline.enqueue(meeting.id, false, true, true));
        {
            let mut queue = inner.queue.lock();
            assert_eq!(queue.len(), 1);
            assert!(
                !queue[0].transcribe,
                "notes were asked for, not a transcript"
            );
            queue.clear();
        }

        inner.park(transcribed(&meeting), Need::Think);
        assert!(pipeline.enqueue(meeting.id, true, true, true));
        assert!(inner.queue.lock()[0].transcribe);

        inner.park(transcribed(&meeting), Need::Transcription);
        assert!(inner.waiting.lock()[0].0.transcribe);
    }

    #[test]
    fn a_job_waiting_for_think_goes_on_when_automatic_summaries_are_turned_off() {
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            auto_summarize: true,
            ..Default::default()
        };
        let (_dir, pipeline, meeting) = stopped(settings.clone());
        let inner = &pipeline.inner;
        let other = Uuid::new_v4();
        inner.park(transcribed(&meeting), Need::Think);
        inner.park(
            Job {
                meeting_id: other,
                summary_follows_setting: false,
                ..transcribed(&meeting)
            },
            Need::Think,
        );

        inner.requeue_ready();
        assert!(inner.queue.lock().is_empty(), "the model is still missing");
        assert_eq!(inner.waiting.lock().len(), 2);

        *inner.shared.settings.write() = lokalbot_core::Settings {
            auto_summarize: false,
            ..settings
        };
        inner.requeue_ready();
        let queued: Vec<Uuid> = inner
            .queue
            .lock()
            .iter()
            .map(|job| job.meeting_id)
            .collect();
        assert_eq!(queued, [meeting.id], "it has nothing left to wait for");
        let waiting: Vec<Uuid> = inner
            .waiting
            .lock()
            .iter()
            .map(|(job, _)| job.meeting_id)
            .collect();
        assert_eq!(waiting, [other], "a summary someone asked for still waits");
    }

    #[test]
    fn a_new_request_takes_over_a_job_parked_for_models() {
        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let settings = lokalbot_core::Settings {
            auto_transcribe: false,
            ..Default::default()
        };
        let pipeline = pipeline(library, settings);
        pipeline.stop();
        std::thread::sleep(IDLE_WAKE * 4);
        let parked = Job {
            meeting_id: meeting.id,
            transcribe: true,
            summarize: false,
            summary_follows_setting: true,
            user_initiated: false,
            resumed: true,
        };
        pipeline
            .inner
            .waiting
            .lock()
            .push((parked, Need::Transcription));

        assert!(pipeline.enqueue(meeting.id, false, true, true));
        assert!(pipeline.inner.waiting.lock().is_empty());
        let queue = pipeline.inner.queue.lock();
        assert_eq!(queue.len(), 1);
        let job = &queue[0];
        assert!(job.transcribe, "the parked work is kept");
        assert!(job.summarize && job.user_initiated && !job.summary_follows_setting);
    }

    #[test]
    fn multi_stt_reconcile_and_vocab_guard_preserve_alignment() {
        assert!(!lokalbot_speech::models::accepts_vocabulary(
            lokalbot_speech::models::WHISPER_TURBO_MODEL
        ));
        let dir = tempfile::tempdir().unwrap();
        let meeting =
            lokalbot_core::Meeting::new("Test", "Zoom", chrono::Utc::now(), "2026/11/test");
        let seg = |start: f64, end: f64, speaker: &str, text: &str| lokalbot_core::Segment {
            start,
            end,
            speaker: speaker.to_string(),
            text: text.to_string(),
            confidence: None,
            timing_precision: Some(lokalbot_core::TimingPrecision::Span),
            attribution: None,
        };
        let multi = |id: usize, start: f64, end: f64, p: &str, b: &str, h: &str| {
            lokalbot_speech::MultiSttSegment {
                segment_id: id,
                start,
                end,
                speaker: "them 1".to_string(),
                primary: p.to_string(),
                tr_backup: b.to_string(),
                fast_helper: h.to_string(),
                timing_precision: Some(lokalbot_core::TimingPrecision::Span),
                attribution: None,
            }
        };

        let mut transcript = Transcript {
            segments: vec![
                seg(0.0, 2.0, "them 1", "Kubernetes"),
                seg(3.0, 5.0, "them 1", "gerçek cümle"),
            ],
            ..Default::default()
        };
        let previous = vec![
            multi(0, 0.0, 2.0, "Kubernetes", "kube", "k"),
            multi(1, 3.0, 5.0, "gerçek cümle", "backup", "helper"),
        ];

        let primary_id = lokalbot_speech::models::WHISPER_TURBO_MODEL;
        let prompt = Some("Kubernetes");
        let guarded_prompt = lokalbot_speech::models::accepts_vocabulary(primary_id)
            .then_some(prompt)
            .flatten();
        assert_eq!(guarded_prompt, None);
        transcript =
            finished_transcript(&meeting, dir.path(), None, transcript, guarded_prompt).unwrap();
        assert_eq!(transcript.segments.len(), 2);

        let range = lokalbot_core::ContentRange {
            start: 2.5,
            end: 6.0,
        };
        range.apply(&mut transcript);
        assert_eq!(transcript.segments.len(), 1);
        let reconciled = reconcile_multi_segments(&transcript, previous);
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].segment_id, 0);
        assert_eq!(reconciled[0].primary, "gerçek cümle");
        assert_eq!(reconciled[0].tr_backup, "backup");
        assert_eq!(reconciled[0].fast_helper, "helper");
    }

    #[test]
    fn post_correction_stage_updates_transcript_and_writes_audit_json() {
        use lokalbot_ai::post_correction::PostCorrectionConfig;
        use lokalbot_core::model::transcript::{Segment, Transcript};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                let _ = reader.read_exact(&mut buf);
                let req_json: serde_json::Value =
                    serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
                let model = req_json["model"].as_str().unwrap_or("");
                let content = match model {
                    "gemma-4-31B-it" => {
                        r#"{"candidates":[{"segment_id":1,"wrong_in_primary":"videolardan","suggested_fix":"üzerinden","supported_by":"zipformer-oayk","reason":"phonetic"}]}"#
                    }
                    "gpt-oss-120b" => r#"{"candidates":[]}"#,
                    "Qwen3.8-27B" => {
                        r#"{"segments":[{"segment_id":1,"text":"sistem üzerinden çalışır"}]}"#
                    }
                    _ => r#"{"candidates":[]}"#,
                };
                let body = serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": content },
                        "finish_reason": "stop"
                    }]
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let mut settings = lokalbot_core::Settings::default();
        let pc_cfg = PostCorrectionConfig {
            enabled: true,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "sk-pipeline-secret".into(),
            ..Default::default()
        };
        crate::post_correction::set_post_correction_config(&mut settings, Some(&pc_cfg));
        crate::post_correction::set_save_audit_log(&mut settings, true);
        crate::post_correction::set_local_llama_server_disabled(&mut settings, true);

        let stages = Stages::default();
        let seen = stages.clone();
        let shared = Shared::for_tests(library.clone(), settings.clone(), move |event| {
            if let EngineEvent::Processing { stage, .. } = event {
                seen.lock().push(stage);
            }
        });
        let models = crate::models::Models::new(shared.clone());
        let pipeline = Pipeline::start(shared, models);

        let transcript = Transcript {
            segments: vec![Segment {
                start: 1.0,
                end: 4.0,
                speaker: "Speaker 1".into(),
                text: "sistem videolardan çalışır".into(),
                confidence: Some(0.9),
                timing_precision: None,
                attribution: None,
            }],
            ..Default::default()
        };
        let corrected = pipeline
            .inner
            .post_correct_transcript(&meeting, &settings, transcript, None)
            .unwrap();
        pipeline.stop();

        assert_eq!(corrected.segments.len(), 1);
        assert_eq!(corrected.segments[0].text, "sistem üzerinden çalışır");
        assert_eq!(corrected.segments[0].start, 1.0);
        assert_eq!(corrected.segments[0].end, 4.0);
        assert_eq!(corrected.segments[0].speaker, "Speaker 1");
        assert!(stages.lock().contains(&Some(Stage::PostCorrecting)));
        let audit_path = library
            .meeting_dir(&meeting)
            .join(crate::post_correction::AUDIT_FILE_NAME);
        assert!(audit_path.is_file());
        let audit_text = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!audit_text.contains("sk-pipeline-secret"));
    }

    #[test]
    fn post_correction_stage_omits_audit_json_when_save_audit_log_is_false() {
        use lokalbot_ai::post_correction::PostCorrectionConfig;
        use lokalbot_core::model::transcript::{Segment, Transcript};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                let _ = reader.read_exact(&mut buf);
                let req_json: serde_json::Value =
                    serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
                let model = req_json["model"].as_str().unwrap_or("");
                let content = match model {
                    "Qwen3.8-27B" => r#"{"segments":[{"segment_id":1,"text":"temiz metin"}]}"#,
                    _ => r#"{"candidates":[]}"#,
                };
                let body = serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": content },
                        "finish_reason": "stop"
                    }]
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let mut settings = lokalbot_core::Settings::default();
        let pc_cfg = PostCorrectionConfig {
            enabled: true,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "sk-no-audit".into(),
            support_critic_models: Vec::new(),
            ..Default::default()
        };
        crate::post_correction::set_post_correction_config(&mut settings, Some(&pc_cfg));
        crate::post_correction::set_save_audit_log(&mut settings, false);

        let stages = Stages::default();
        let seen = stages.clone();
        let shared = Shared::for_tests(library.clone(), settings.clone(), move |event| {
            if let EngineEvent::Processing { stage, .. } = event {
                seen.lock().push(stage);
            }
        });
        let models = crate::models::Models::new(shared.clone());
        let pipeline = Pipeline::start(shared, models);

        let transcript = Transcript {
            segments: vec![Segment {
                start: 0.0,
                end: 2.0,
                speaker: "Speaker 1".into(),
                text: "ham metin".into(),
                confidence: Some(0.8),
                timing_precision: None,
                attribution: None,
            }],
            ..Default::default()
        };
        let corrected = pipeline
            .inner
            .post_correct_transcript(&meeting, &settings, transcript, None)
            .unwrap();
        pipeline.stop();

        assert_eq!(corrected.segments[0].text, "temiz metin");
        assert!(stages.lock().contains(&Some(Stage::PostCorrecting)));
        let audit_path = library
            .meeting_dir(&meeting)
            .join(crate::post_correction::AUDIT_FILE_NAME);
        assert!(!audit_path.exists());
    }

    #[test]
    fn post_correction_stage_skips_when_disabled_or_unconfigured() {
        use lokalbot_ai::post_correction::PostCorrectionConfig;
        use lokalbot_core::model::transcript::{Segment, Transcript};
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        std::thread::spawn(move || {
            for _stream in listener.incoming().flatten() {
                hits_srv.fetch_add(1, Ordering::SeqCst);
            }
        });

        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let stages = Stages::default();
        let seen = stages.clone();
        let settings_unconfigured = lokalbot_core::Settings::default();
        let shared = Shared::for_tests(
            library.clone(),
            settings_unconfigured.clone(),
            move |event| {
                if let EngineEvent::Processing { stage, .. } = event {
                    seen.lock().push(stage);
                }
            },
        );
        let models = crate::models::Models::new(shared.clone());
        let pipeline = Pipeline::start(shared, models);

        let transcript = Transcript {
            segments: vec![Segment {
                start: 0.0,
                end: 1.5,
                speaker: "Speaker 1".into(),
                text: "unchanged".into(),
                confidence: None,
                timing_precision: None,
                attribution: None,
            }],
            ..Default::default()
        };

        let out_unconfigured = pipeline
            .inner
            .post_correct_transcript(&meeting, &settings_unconfigured, transcript.clone(), None)
            .unwrap();
        assert_eq!(out_unconfigured.segments[0].text, "unchanged");

        let mut settings_disabled = lokalbot_core::Settings::default();
        let disabled_cfg = PostCorrectionConfig {
            enabled: false,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            ..Default::default()
        };
        crate::post_correction::set_post_correction_config(
            &mut settings_disabled,
            Some(&disabled_cfg),
        );
        let out_disabled = pipeline
            .inner
            .post_correct_transcript(&meeting, &settings_disabled, transcript, None)
            .unwrap();
        assert_eq!(out_disabled.segments[0].text, "unchanged");
        pipeline.stop();

        assert!(!stages.lock().contains(&Some(Stage::PostCorrecting)));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn single_mode_bypasses_multi_stt_runner_and_keeps_single_model_requirements() {
        let (_dir, library) = library();
        let settings = lokalbot_core::Settings {
            transcription_mode: lokalbot_core::config_profile::TranscriptionMode::Single,
            transcription_model: "parakeet-v3".into(),
            ..Default::default()
        };
        let single_cfg = crate::speech::MultiSttConfig::from_settings(&settings);
        assert!(!single_cfg.enabled);

        let shared = Shared::for_tests(library.clone(), settings, |_| {});
        let models = crate::models::Models::new(shared);
        assert!(models.multi_stt_config().is_none());
        let reqs = models.meeting_requirements(false);
        assert!(reqs.contains(&"parakeet-v3".to_string()));
        assert!(!reqs.contains(&lokalbot_speech::models::WHISPER_TURBO_TR_KHAN_MODEL.to_string()));
        assert!(!reqs.contains(&lokalbot_speech::models::ZIPFORMER_OAYK_MODEL.to_string()));

        let multi_settings = lokalbot_core::Settings {
            transcription_mode:
                lokalbot_core::config_profile::TranscriptionMode::AsymmetricMultiStt,
            ..Default::default()
        };
        let multi_shared = Shared::for_tests(library, multi_settings, |_| {});
        let multi_models = crate::models::Models::new(multi_shared);
        assert!(multi_models.multi_stt_config().is_some());
        let multi_reqs = multi_models.meeting_requirements(false);
        assert!(multi_reqs.contains(&lokalbot_speech::models::WHISPER_TURBO_MODEL.to_string()));
        assert!(
            multi_reqs.contains(&lokalbot_speech::models::WHISPER_TURBO_TR_KHAN_MODEL.to_string())
        );
        assert!(multi_reqs.contains(&lokalbot_speech::models::ZIPFORMER_OAYK_MODEL.to_string()));
    }

    #[test]
    fn pipeline_post_correction_receives_multi_stt_hypotheses() {
        use lokalbot_ai::post_correction::PostCorrectionConfig;
        use lokalbot_core::model::transcript::{Segment, Transcript};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let saw_tr_backup = Arc::new(AtomicBool::new(false));
        let saw_fast_helper = Arc::new(AtomicBool::new(false));
        let b_flag = saw_tr_backup.clone();
        let h_flag = saw_fast_helper.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                let _ = reader.read_exact(&mut buf);
                let req_json: serde_json::Value =
                    serde_json::from_slice(&buf).unwrap_or(serde_json::Value::Null);
                let payload = req_json.to_string();
                if payload.contains("LocalBOT kurulumu") {
                    b_flag.store(true, Ordering::SeqCst);
                }
                if payload.contains("lokalbot kurulumu") {
                    h_flag.store(true, Ordering::SeqCst);
                }
                let model = req_json["model"].as_str().unwrap_or("");
                let content = match model {
                    "Qwen3.8-27B" => {
                        r#"{"segments":[{"segment_id":1,"text":"LokalBot kurulumu"}]}"#
                    }
                    _ => r#"{"candidates":[]}"#,
                };
                let body = serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": content },
                        "finish_reason": "stop"
                    }]
                })
                .to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        let (_dir, library) = library();
        let meeting = finished_without_audio(&library);
        let settings = lokalbot_core::Settings {
            post_correction_enabled: true,
            post_correction_base_url: format!("http://127.0.0.1:{port}/v1"),
            post_correction_api_key: "sk-typed-pc-secret".into(),
            transcription_save_audit_log: true,
            ..Default::default()
        };
        let _defaults = PostCorrectionConfig::default();

        let shared = Shared::for_tests(library.clone(), settings.clone(), |_| {});
        let models = crate::models::Models::new(shared.clone());
        let pipeline = Pipeline::start(shared, models);

        let transcript = Transcript {
            segments: vec![Segment {
                start: 0.0,
                end: 2.0,
                speaker: "Speaker 1".into(),
                text: "lokal bot kurulumu".into(),
                confidence: None,
                timing_precision: None,
                attribution: None,
            }],
            ..Default::default()
        };
        let multi = vec![lokalbot_speech::MultiSttSegment {
            segment_id: 0,
            start: 0.0,
            end: 2.0,
            speaker: "Speaker 1".into(),
            primary: "lokal bot kurulumu".into(),
            tr_backup: "LocalBOT kurulumu".into(),
            fast_helper: "lokalbot kurulumu".into(),
            timing_precision: None,
            attribution: None,
        }];
        let corrected = pipeline
            .inner
            .post_correct_transcript(&meeting, &settings, transcript, Some(&multi))
            .unwrap();
        pipeline.stop();

        assert_eq!(corrected.segments[0].text, "LokalBot kurulumu");
        assert!(saw_tr_backup.load(Ordering::SeqCst));
        assert!(saw_fast_helper.load(Ordering::SeqCst));
        let audit_path = library
            .meeting_dir(&meeting)
            .join(crate::post_correction::AUDIT_FILE_NAME);
        assert!(audit_path.is_file());
    }

    #[test]
    fn transcription_ready_ignores_blocked_remote_model_in_multi_stt_mode() {
        let (_dir, library) = library();
        for id in [
            lokalbot_speech::models::WHISPER_TURBO_MODEL,
            lokalbot_speech::models::WHISPER_TURBO_TR_KHAN_MODEL,
            lokalbot_speech::models::ZIPFORMER_OAYK_MODEL,
            lokalbot_speech::models::VAD_MODEL,
        ] {
            let model = lokalbot_speech::models::find(id).unwrap();
            let dir = library.paths().models_dir().join("speech").join(model.id);
            for file in model.files {
                let path = lokalbot_speech::models::model_file(library.paths(), model, file);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"not a model").unwrap();
            }
            lokalbot_speech::models::write_bundled_marker(&dir, model).unwrap();
        }

        let multi_settings = lokalbot_core::Settings {
            transcription_mode:
                lokalbot_core::config_profile::TranscriptionMode::AsymmetricMultiStt,
            transcription_model: lokalbot_core::remote_transcription::MODEL_ID.into(),
            multi_speaker_diarization: false,
            ..Default::default()
        };
        let multi_shared = Shared::for_tests(library.clone(), multi_settings, |_| {});
        let multi_models = crate::models::Models::new(multi_shared.clone());
        assert!(multi_models.transcription_server_blocked());
        let multi_pipeline = Pipeline::start(multi_shared, multi_models);
        assert!(multi_pipeline.transcription_ready());
        assert!(multi_pipeline.inner.ready(Need::Transcription));
        multi_pipeline.stop();

        let single_settings = lokalbot_core::Settings {
            transcription_mode: lokalbot_core::config_profile::TranscriptionMode::Single,
            transcription_model: lokalbot_core::remote_transcription::MODEL_ID.into(),
            multi_speaker_diarization: false,
            ..Default::default()
        };
        let single_shared = Shared::for_tests(library, single_settings, |_| {});
        let single_models = crate::models::Models::new(single_shared.clone());
        assert!(single_models.transcription_server_blocked());
        let single_pipeline = Pipeline::start(single_shared, single_models);
        assert!(!single_pipeline.transcription_ready());
        assert!(!single_pipeline.inner.ready(Need::Transcription));
        single_pipeline.stop();
    }
}
