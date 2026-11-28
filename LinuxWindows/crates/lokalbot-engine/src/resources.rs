use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, Local};
use lokalbot_ai::runtime::Role;
use lokalbot_platform::process_usage::process_usage;
use parking_lot::Mutex;

use crate::Engine;

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedModel {
    pub role: String,
    pub label: String,
    pub memory_bytes: Option<u64>,
    pub in_use: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveModelRun {
    pub kind: String,
    pub model: String,
    pub detail: Option<String>,
    pub started_at: DateTime<Local>,
    pub elapsed_seconds: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletedModelRun {
    pub kind: String,
    pub model: String,
    pub detail: Option<String>,
    pub started_at: DateTime<Local>,
    pub finished_at: DateTime<Local>,
    pub elapsed_seconds: f64,
}

const MAXIMUM_RUN_HISTORY: usize = 200;

struct ActiveEntry {
    id: u64,
    kind: String,
    model: String,
    detail: Option<String>,
    started_at: DateTime<Local>,
    started_instant: Instant,
}

#[derive(Default)]
pub struct ActiveModels {
    next_id: AtomicU64,
    entries: Mutex<Vec<ActiveEntry>>,
    history: Mutex<VecDeque<CompletedModelRun>>,
}

impl ActiveModels {
    pub fn start(
        &self,
        kind: impl Into<String>,
        model: impl Into<String>,
        detail: Option<String>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = ActiveEntry {
            id,
            kind: kind.into(),
            model: model.into(),
            detail,
            started_at: Local::now(),
            started_instant: Instant::now(),
        };
        self.entries.lock().push(entry);
        id
    }

    #[allow(dead_code)]
    pub fn update(&self, id: u64, model: impl Into<String>, detail: Option<String>) {
        let model = model.into();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.model = model;
            entry.detail = detail;
        }
    }

    pub fn update_detail(&self, id: u64, detail: Option<String>) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.detail = detail;
        }
    }

    pub fn finish(&self, id: u64) {
        let removed = {
            let mut entries = self.entries.lock();
            entries
                .iter()
                .position(|e| e.id == id)
                .map(|idx| entries.remove(idx))
        };
        if let Some(entry) = removed {
            let mut history = self.history.lock();
            history.push_front(CompletedModelRun {
                kind: entry.kind,
                model: entry.model,
                detail: entry.detail,
                started_at: entry.started_at,
                finished_at: Local::now(),
                elapsed_seconds: entry.started_instant.elapsed().as_secs_f64(),
            });
            history.truncate(MAXIMUM_RUN_HISTORY);
        }
    }

    pub fn completed(&self) -> Vec<CompletedModelRun> {
        self.history.lock().iter().cloned().collect()
    }

    pub fn snapshot(&self) -> Vec<ActiveModelRun> {
        self.entries
            .lock()
            .iter()
            .map(|entry| ActiveModelRun {
                kind: entry.kind.clone(),
                model: entry.model.clone(),
                detail: entry.detail.clone(),
                started_at: entry.started_at,
                elapsed_seconds: entry.started_instant.elapsed().as_secs_f64(),
            })
            .collect()
    }
}

pub(crate) struct ModelRunGuard<'a> {
    tracker: &'a ActiveModels,
    id: u64,
}

impl<'a> ModelRunGuard<'a> {
    pub(crate) fn new(
        tracker: &'a ActiveModels,
        kind: impl Into<String>,
        model: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        let id = tracker.start(kind, model, detail);
        Self { tracker, id }
    }

    #[allow(dead_code)]
    pub(crate) fn update(&self, model: impl Into<String>, detail: Option<String>) {
        self.tracker.update(self.id, model, detail);
    }

    pub(crate) fn update_detail(&self, detail: Option<String>) {
        self.tracker.update_detail(self.id, detail);
    }
}

impl Drop for ModelRunGuard<'_> {
    fn drop(&mut self) {
        self.tracker.finish(self.id);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceSnapshot {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub models: Vec<LoadedModel>,
    pub active_runs: Vec<ActiveModelRun>,
    pub completed_runs: Vec<CompletedModelRun>,
    pub generations: Vec<GenerationMetric>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GenerationMetric {
    pub label: String,
    pub seconds: f64,
    pub approx_tokens: usize,
    pub started_at: DateTime<Local>,
}

impl GenerationMetric {
    pub fn tokens_per_second(&self) -> f64 {
        if self.seconds > 0.0 {
            self.approx_tokens as f64 / self.seconds
        } else {
            0.0
        }
    }
}

#[derive(Default)]
pub struct GenerationMetrics {
    recent: Mutex<VecDeque<GenerationMetric>>,
}

const MAXIMUM_METRICS: usize = 20;

impl GenerationMetrics {
    pub fn record(&self, label: String, seconds: f64, text: &str) {
        let duration_ms = (seconds.max(0.0) * 1000.0).round() as i64;
        let started_at = Local::now() - chrono::Duration::milliseconds(duration_ms);
        let mut recent = self.recent.lock();
        recent.push_front(GenerationMetric {
            label,
            seconds,
            approx_tokens: estimated_tokens(text),
            started_at,
        });
        recent.truncate(MAXIMUM_METRICS);
    }

    pub fn recent(&self) -> Vec<GenerationMetric> {
        self.recent.lock().iter().cloned().collect()
    }
}

fn estimated_tokens(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| ((word.chars().count() as f64 / 4.0).round() as usize).max(1))
        .sum()
}

impl ResourceSnapshot {
    pub fn model_memory_bytes(&self) -> u64 {
        self.models.iter().filter_map(|m| m.memory_bytes).sum()
    }
}

#[derive(Default)]
pub struct ResourceSampler {
    last: Option<(Instant, f64)>,
}

fn role_title(role: Role) -> &'static str {
    match role {
        Role::Main => "Think",
        Role::Embedder => "Embeddings",
        Role::Autocomplete => "Autocomplete",
        Role::MediaEmbedder => "Files and screen pictures",
    }
}

impl ResourceSampler {
    pub fn sample(&mut self, engine: &Engine) -> ResourceSnapshot {
        let mut cpu_seconds = 0.0;
        let mut memory = None;
        let mut add = |pid: u32| {
            let usage = process_usage(pid)?;
            cpu_seconds += usage.cpu_seconds;
            memory = Some(memory.unwrap_or(0) + usage.memory_bytes);
            Some(usage.memory_bytes)
        };
        add(std::process::id());
        let mut models = Vec::new();
        for status in engine.runtime().statuses() {
            let Some(pid) = status.pid.filter(|_| status.running) else {
                continue;
            };
            let label = status
                .model_id
                .as_deref()
                .and_then(lokalbot_ai::catalog::entry)
                .map(|entry| entry.display_name.to_string())
                .or(status.model_id.clone())
                .unwrap_or_default();
            models.push(LoadedModel {
                role: role_title(status.role).to_string(),
                label,
                memory_bytes: add(pid),
                in_use: status.leases > 0,
            });
        }
        for (role, label) in engine.loaded_speech_models() {
            models.push(LoadedModel {
                role: role.to_string(),
                label,
                memory_bytes: None,
                in_use: false,
            });
        }
        let now = Instant::now();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;
        let cpu_percent = self.last.and_then(|(at, before)| {
            cpu_percent(
                before,
                cpu_seconds,
                now.duration_since(at).as_secs_f64(),
                cores,
            )
        });
        self.last = Some((now, cpu_seconds));
        ResourceSnapshot {
            cpu_percent,
            memory_bytes: memory,
            models,
            active_runs: engine.active_models(),
            completed_runs: engine.completed_models(),
            generations: engine.recent_generations(),
        }
    }
}

fn cpu_percent(before: f64, after: f64, elapsed: f64, cores: f64) -> Option<f64> {
    if elapsed <= 0.0 || after < before {
        return None;
    }
    Some(((after - before) / (elapsed * cores) * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_keep_the_last_twenty_newest_first() {
        let metrics = GenerationMetrics::default();
        for index in 0..25 {
            metrics.record(format!("Summary {index}"), 2.0, "four five, sixteen");
        }
        let recent = metrics.recent();
        assert_eq!(recent.len(), MAXIMUM_METRICS);
        assert_eq!(recent[0].label, "Summary 24");
        assert_eq!(recent[0].approx_tokens, 4);
        assert_eq!(recent[0].tokens_per_second(), 2.0);
        let idle = GenerationMetric {
            label: String::new(),
            seconds: 0.0,
            approx_tokens: 5,
            started_at: Local::now(),
        };
        assert_eq!(idle.tokens_per_second(), 0.0);
        assert_eq!(estimated_tokens("  \n"), 0);
    }

    #[test]
    fn active_models_track_lifecycle() {
        let tracker = ActiveModels::default();
        assert!(tracker.snapshot().is_empty());
        let id = tracker.start("STT", "whisper-turbo", Some("Transcribing".into()));
        let active = tracker.snapshot();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].kind, "STT");
        assert_eq!(active[0].model, "whisper-turbo");
        assert_eq!(active[0].detail.as_deref(), Some("Transcribing"));
        tracker.update(id, "whisper-turbo-tr", Some("50%".into()));
        let updated = tracker.snapshot();
        assert_eq!(updated[0].model, "whisper-turbo-tr");
        assert_eq!(updated[0].detail.as_deref(), Some("50%"));
        tracker.finish(id);
        assert!(tracker.snapshot().is_empty());
        let completed = tracker.completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].kind, "STT");
        assert_eq!(completed[0].model, "whisper-turbo-tr");
        assert_eq!(completed[0].detail.as_deref(), Some("50%"));
        assert!(completed[0].elapsed_seconds >= 0.0);
        let id2 = tracker.start("Summary", "Qwen3.5", None);
        tracker.finish(id2);
        let completed2 = tracker.completed();
        assert_eq!(completed2.len(), 2);
        assert_eq!(completed2[0].kind, "Summary");
        assert_eq!(completed2[1].kind, "STT");
    }

    #[test]
    fn cpu_is_shared_across_cores() {
        assert_eq!(cpu_percent(1.0, 3.0, 2.0, 4.0), Some(25.0));
        assert_eq!(cpu_percent(0.0, 100.0, 1.0, 1.0), Some(100.0));
        assert_eq!(cpu_percent(5.0, 3.0, 2.0, 4.0), None);
        assert_eq!(cpu_percent(1.0, 3.0, 0.0, 4.0), None);
    }

    #[test]
    fn model_memory_adds_what_is_known() {
        let model = |memory_bytes| LoadedModel {
            role: "Think".into(),
            label: "m".into(),
            memory_bytes,
            in_use: false,
        };
        let snapshot = ResourceSnapshot {
            models: vec![model(Some(5)), model(None), model(Some(7))],
            ..Default::default()
        };
        assert_eq!(snapshot.model_memory_bytes(), 12);
    }
}
