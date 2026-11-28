pub mod critic;
pub mod judge;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use lokalbot_core::settings::{OpenRouterDataPolicy, ReasoningLevel, SummarizerBackend};

use crate::error::{AiError, ai_error};
use crate::http;
use crate::sanitize;
use crate::think::{
    ChatRequest, Dialect, RequestReasoning, chat_body, consent, parse_chat_completion,
    resolve_api_key, trimmed_base, with_data_policy,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiSttSegment {
    pub segment_id: usize,
    pub start: f64,
    pub end: f64,
    pub speaker: String,
    pub primary_text: String,
    #[serde(default)]
    pub tr_backup_text: String,
    #[serde(default)]
    pub fast_helper_text: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PostCorrectionConfig {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub primary_judge_model: String,
    pub support_critic_models: Vec<String>,
    pub segments_per_batch: usize,
    pub request_timeout_sec: u64,
    pub max_concurrent_batches: usize,
    pub continue_on_critic_failure: bool,
}

impl Default for PostCorrectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            api_key: String::new(),
            primary_judge_model: "Qwen3.8-27B".into(),
            support_critic_models: vec!["gemma-4-31B-it".into()],
            segments_per_batch: 5,
            request_timeout_sec: 120,
            max_concurrent_batches: 2,
            continue_on_critic_failure: true,
        }
    }
}

impl fmt::Debug for PostCorrectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let masked_key = if self.api_key.trim().is_empty() {
            "<empty>"
        } else {
            "[REDACTED]"
        };
        f.debug_struct("PostCorrectionConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("api_key", &masked_key)
            .field("primary_judge_model", &self.primary_judge_model)
            .field("support_critic_models", &self.support_critic_models)
            .field("segments_per_batch", &self.segments_per_batch)
            .field("request_timeout_sec", &self.request_timeout_sec)
            .field("max_concurrent_batches", &self.max_concurrent_batches)
            .field(
                "continue_on_critic_failure",
                &self.continue_on_critic_failure,
            )
            .finish()
    }
}

impl PostCorrectionConfig {
    pub fn effective_batch_size(&self) -> usize {
        self.segments_per_batch.clamp(1, 32)
    }

    pub fn effective_concurrency(&self) -> usize {
        self.max_concurrent_batches.max(1)
    }

    pub fn effective_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_sec.max(1))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionCandidate {
    pub segment_id: usize,
    pub wrong_in_primary: String,
    pub suggested_fix: String,
    #[serde(default)]
    pub supported_by: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CriticModelResult {
    pub model: String,
    pub candidates: Vec<CorrectionCandidate>,
    pub used_line_diff_fallback: bool,
    pub response_format_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentAudit {
    pub segment_id: usize,
    pub start: f64,
    pub end: f64,
    pub speaker: String,
    pub primary_text: String,
    pub tr_backup_text: String,
    pub fast_helper_text: String,
    pub critic_candidates: Vec<CriticModelResult>,
    pub final_text: String,
    pub changed: bool,
    pub judge_fallback: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostCorrectionAudit {
    pub schema_version: u32,
    pub created_at: String,
    pub primary_judge_model: String,
    pub support_critic_models: Vec<String>,
    pub segments_per_batch: usize,
    pub segments: Vec<SegmentAudit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PostCorrectionOutput {
    pub segments: Vec<MultiSttSegment>,
    pub texts: Vec<String>,
    pub audit: PostCorrectionAudit,
}

impl PostCorrectionOutput {
    pub fn apply_to_transcript(&self, transcript: &mut lokalbot_core::Transcript) -> Result<()> {
        if self.texts.len() != transcript.segments.len() {
            return Err(AiError::BadResponse(format!(
                "post-correction returned {} segments for transcript with {} segments",
                self.texts.len(),
                transcript.segments.len()
            ))
            .into());
        }
        for (segment, corrected_text) in transcript.segments.iter_mut().zip(&self.texts) {
            segment.text = corrected_text.clone();
        }
        Ok(())
    }
}

pub(crate) fn redact_secrets(text: &str, secrets: &[&str]) -> String {
    let mut out = sanitize::redact(text);
    for secret in secrets {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            out = out.replace(trimmed, "[REDACTED]");
        }
        if !secret.is_empty() && *secret != trimmed {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    out
}

pub(crate) struct CompletionCall {
    pub text: String,
    pub response_format_fallback: bool,
}

pub fn is_local_catalog_model(model: &str) -> bool {
    let trimmed = model.trim();
    !trimmed.is_empty()
        && crate::catalog::entry(trimmed)
            .is_some_and(|entry| entry.kind == crate::catalog::ModelKind::Text)
}

pub fn requires_remote_endpoint(config: &PostCorrectionConfig) -> bool {
    !is_local_catalog_model(&config.primary_judge_model)
        || config
            .support_critic_models
            .iter()
            .any(|m| !m.trim().is_empty() && !is_local_catalog_model(m))
}

pub struct PostCorrector {
    pub(crate) config: PostCorrectionConfig,
    base_url: String,
    resolved_api_key: Option<String>,
    pub(crate) vocabulary: Vec<String>,
    data_policy: OpenRouterDataPolicy,
    schema_unsupported: Mutex<HashSet<String>>,
    runtime: Option<crate::runtime::RuntimeManager>,
    reasoning_level: ReasoningLevel,
}

impl PostCorrector {
    pub fn new(config: PostCorrectionConfig, approved_origins: Vec<String>) -> Result<Self> {
        if config.primary_judge_model.trim().is_empty() {
            return Err(AiError::NoModel.into());
        }
        let base_url = trimmed_base(&config.base_url);
        if requires_remote_endpoint(&config) {
            if base_url.is_empty() {
                return Err(AiError::Unavailable("Post-correction base_url is empty.".into()).into());
            }
            consent(&base_url, &approved_origins)?;
        }
        let resolved_api_key = resolve_api_key(Some(&config.api_key), None);
        Ok(Self {
            config,
            base_url,
            resolved_api_key,
            vocabulary: Vec::new(),
            data_policy: OpenRouterDataPolicy::PrivateOnly,
            schema_unsupported: Mutex::new(HashSet::new()),
            runtime: None,
            reasoning_level: ReasoningLevel::Automatic,
        })
    }

    pub fn with_local_runtime(
        mut self,
        runtime: crate::runtime::RuntimeManager,
        level: ReasoningLevel,
    ) -> Self {
        self.runtime = Some(runtime);
        self.reasoning_level = level;
        self
    }

    pub fn with_vocabulary(mut self, terms: Vec<String>) -> Self {
        self.vocabulary = terms
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        self
    }

    pub fn with_openrouter_data_policy(mut self, policy: OpenRouterDataPolicy) -> Self {
        self.data_policy = policy;
        self
    }

    pub fn with_keyring_fallback(mut self, keyring_key: Option<String>) -> Self {
        self.resolved_api_key = resolve_api_key(Some(&self.config.api_key), keyring_key);
        self
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        let resolved = self.resolved_api_key.as_deref().unwrap_or("");
        redact_secrets(text, &[&self.config.api_key, resolved])
    }

    pub(crate) fn complete_json_or_text(
        &self,
        model: &str,
        request: &ChatRequest,
    ) -> Result<CompletionCall> {
        if is_local_catalog_model(model) {
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                AiError::Unavailable(
                    "Local model runtime is not available for post-correction.".into(),
                )
            })?;
            let local_settings = lokalbot_core::Settings {
                summarizer_backend: SummarizerBackend::BuiltIn,
                built_in_model_id: model.trim().to_string(),
                think_reasoning_level: self.reasoning_level,
                ..Default::default()
            };
            let client = crate::think::ThinkClient::new(runtime, &local_settings)?;
            let text = client.complete(request)?;
            return Ok(CompletionCall {
                text,
                response_format_fallback: false,
            });
        }
        let timeout = self.config.effective_timeout();
        let started = Instant::now();
        let dialect = Dialect::inferred(&self.base_url);
        let url = format!("{}/chat/completions", self.base_url);
        let bearer = self.resolved_api_key.as_deref();

        let schema_known_unsupported =
            request.json_schema.is_some() && self.schema_unsupported.lock().contains(model);

        let mut use_schema = request.json_schema.is_some() && !schema_known_unsupported;
        let mut response_format_fallback = schema_known_unsupported;

        let mut attempts = 0usize;
        loop {
            let effective_request = if use_schema {
                request.clone()
            } else {
                let mut plain = request.clone();
                plain.json_schema = None;
                plain
            };
            let body = with_data_policy(
                chat_body(
                    dialect,
                    model,
                    &effective_request,
                    RequestReasoning {
                        level: None,
                        allowance: 0,
                        default_off: false,
                        takes_effort: false,
                    },
                    &[],
                ),
                self.data_policy,
            );
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(AiError::ServerUnreachable(self.base_url.clone()).into());
            }
            let agent = http::agent(&self.base_url, remaining);
            let reply = http::post_json(&agent, &url, bearer, &body).map_err(|err| {
                let msg = self.redact(&format!("{err:#}"));
                anyhow::anyhow!(msg)
            })?;

            if reply.ok() {
                let json: Value = reply.json()?;
                let completion = parse_chat_completion(&json)?;
                return Ok(CompletionCall {
                    text: completion.text,
                    response_format_fallback,
                });
            }

            if use_schema && matches!(reply.status, 400 | 422 | 501) {
                self.schema_unsupported.lock().insert(model.to_string());
                use_schema = false;
                response_format_fallback = true;
                continue;
            }

            let status_err = reply.status_error();
            let err_boxed: anyhow::Error = status_err.into();
            let retryable = ai_error(&err_boxed).is_some_and(AiError::is_retryable);
            if retryable && attempts < 2 {
                attempts += 1;
                let default_wait = Duration::from_millis(200 * attempts as u64);
                let wait = ai_error(&err_boxed)
                    .and_then(AiError::retry_after)
                    .unwrap_or(default_wait);
                let budget_left = timeout.saturating_sub(started.elapsed());
                if budget_left > Duration::from_millis(50) {
                    std::thread::sleep(
                        wait.min(budget_left.saturating_sub(Duration::from_millis(25))),
                    );
                    continue;
                }
            }
            let redacted = self.redact(&format!("{err_boxed:#}"));
            return Err(anyhow::anyhow!(redacted));
        }
    }

    pub fn correct(&self, segments: &[MultiSttSegment]) -> Result<PostCorrectionOutput> {
        if segments.is_empty() {
            return Ok(PostCorrectionOutput {
                segments: Vec::new(),
                texts: Vec::new(),
                audit: PostCorrectionAudit {
                    schema_version: 1,
                    created_at: Utc::now().to_rfc3339(),
                    primary_judge_model: self.config.primary_judge_model.clone(),
                    support_critic_models: self.config.support_critic_models.clone(),
                    segments_per_batch: self.config.effective_batch_size(),
                    segments: Vec::new(),
                    warnings: Vec::new(),
                },
            });
        }

        let batch_size = self.config.effective_batch_size();
        let ranges: Vec<(usize, usize)> = (0..segments.len())
            .step_by(batch_size)
            .map(|start| (start, (start + batch_size).min(segments.len())))
            .collect();

        let batch_count = ranges.len();
        let results: Mutex<Vec<Option<Result<BatchOutput>>>> =
            Mutex::new((0..batch_count).map(|_| None).collect());
        let cursor = Mutex::new(0usize);
        let critic_failures: Mutex<HashMap<String, usize>> = Mutex::new(HashMap::new());
        let aborted = AtomicBool::new(false);
        let workers = self.config.effective_concurrency().min(batch_count);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        if aborted.load(Ordering::Acquire) {
                            break;
                        }
                        let idx = {
                            let mut guard = cursor.lock();
                            if *guard >= batch_count {
                                break;
                            }
                            let current = *guard;
                            *guard += 1;
                            current
                        };
                        let (start, end) = ranges[idx];
                        let outcome =
                            self.process_batch(segments, start, end, &critic_failures);
                        if outcome.is_err() {
                            aborted.store(true, Ordering::Release);
                        }
                        results.lock()[idx] = Some(outcome);
                    }
                });
            }
        });

        let mut ordered_segments = Vec::with_capacity(segments.len());
        let mut ordered_texts = Vec::with_capacity(segments.len());
        let mut audit_segments = Vec::with_capacity(segments.len());
        let mut warnings = Vec::new();

        let mut collected = Vec::with_capacity(batch_count);
        for slot in results.into_inner() {
            match slot {
                Some(Ok(batch_outcome)) => collected.push(batch_outcome),
                Some(Err(err)) => return Err(err),
                None => {}
            }
        }
        if collected.len() != batch_count {
            return Err(anyhow::anyhow!("missing batch result"));
        }
        for batch_outcome in collected {
            ordered_segments.extend(batch_outcome.segments);
            ordered_texts.extend(batch_outcome.texts);
            audit_segments.extend(batch_outcome.audit_segments);
            warnings.extend(batch_outcome.warnings);
        }

        Ok(PostCorrectionOutput {
            segments: ordered_segments,
            texts: ordered_texts,
            audit: PostCorrectionAudit {
                schema_version: 1,
                created_at: Utc::now().to_rfc3339(),
                primary_judge_model: self.config.primary_judge_model.clone(),
                support_critic_models: self.config.support_critic_models.clone(),
                segments_per_batch: batch_size,
                segments: audit_segments,
                warnings,
            },
        })
    }

const CRITIC_MAX_FAILURES: usize = 3;

    fn process_batch(
        &self,
        all: &[MultiSttSegment],
        start: usize,
        end: usize,
        critic_failures: &Mutex<HashMap<String, usize>>,
    ) -> Result<BatchOutput> {
        let batch = &all[start..end];
        let prev_start = start.saturating_sub(2);
        let prev_context = &all[prev_start..start];
        let next_end = (end + 2).min(all.len());
        let next_context = &all[end..next_end];

        let critic_models: Vec<String> = {
            let failures = critic_failures.lock();
            self.config
                .support_critic_models
                .iter()
                .map(|m| m.trim().to_string())
                .filter(|m| {
                    !m.is_empty()
                        && (!self.config.continue_on_critic_failure
                            || failures.get(m).copied().unwrap_or(0) < Self::CRITIC_MAX_FAILURES)
                })
                .collect()
        };

        let critic_results: Vec<CriticModelResult> = if critic_models.len() <= 1 {
            critic_models
                .iter()
                .map(|model| critic::run_batch(self, model, batch, prev_context, next_context))
                .collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = critic_models
                    .iter()
                    .map(|model| {
                        scope.spawn(move || {
                            critic::run_batch(self, model, batch, prev_context, next_context)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("critic thread panicked"))
                    .collect()
            })
        };

        let mut warnings = Vec::new();
        for critic_res in &critic_results {
            if let Some(err) = &critic_res.error {
                let msg = self.redact(&format!("critic {} failed: {}", critic_res.model, err));
                if !self.config.continue_on_critic_failure {
                    return Err(anyhow::anyhow!(msg));
                }
                let count = {
                    let mut failures = critic_failures.lock();
                    let entry = failures.entry(critic_res.model.clone()).or_insert(0);
                    *entry += 1;
                    *entry
                };
                log::warn!("{msg}");
                warnings.push(msg);
                if count == Self::CRITIC_MAX_FAILURES {
                    let skip_msg = format!(
                        "critic {} failed {} times; skipping it for remaining batches",
                        critic_res.model, Self::CRITIC_MAX_FAILURES
                    );
                    log::warn!("{skip_msg}");
                    warnings.push(skip_msg);
                }
            }
        }

        let judge_out = judge::run_batch(self, batch, prev_context, next_context, &critic_results)?;
        if let Some(warn) = judge_out.warning {
            warnings.push(warn);
        }

        let mut out_segments = Vec::with_capacity(batch.len());
        let mut out_texts = Vec::with_capacity(batch.len());
        let mut audit_segments = Vec::with_capacity(batch.len());

        for (seg, final_text) in batch.iter().zip(judge_out.texts) {
            let changed = final_text != seg.primary_text;
            let mut updated_seg = seg.clone();
            updated_seg.primary_text = final_text.clone();
            out_segments.push(updated_seg);
            out_texts.push(final_text.clone());

            let per_segment_critics: Vec<CriticModelResult> = critic_results
                .iter()
                .map(|cr| CriticModelResult {
                    model: cr.model.clone(),
                    candidates: cr
                        .candidates
                        .iter()
                        .filter(|c| c.segment_id == seg.segment_id)
                        .cloned()
                        .collect(),
                    used_line_diff_fallback: cr.used_line_diff_fallback,
                    response_format_fallback: cr.response_format_fallback,
                    error: cr.error.clone(),
                })
                .collect();

            audit_segments.push(SegmentAudit {
                segment_id: seg.segment_id,
                start: seg.start,
                end: seg.end,
                speaker: seg.speaker.clone(),
                primary_text: seg.primary_text.clone(),
                tr_backup_text: seg.tr_backup_text.clone(),
                fast_helper_text: seg.fast_helper_text.clone(),
                critic_candidates: per_segment_critics,
                final_text,
                changed,
                judge_fallback: judge_out.judge_fallback,
            });
        }

        Ok(BatchOutput {
            segments: out_segments,
            texts: out_texts,
            audit_segments,
            warnings,
        })
    }
}

struct BatchOutput {
    segments: Vec<MultiSttSegment>,
    texts: Vec<String>,
    audit_segments: Vec<SegmentAudit>,
    warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::http::testing::{Reply, StubServer};

    fn chat_ok(content: &str) -> Reply {
        Reply::Json(
            200,
            json!({
                "choices": [{
                    "message": { "role": "assistant", "content": content },
                    "finish_reason": "stop"
                }]
            }),
        )
    }

    fn sample_segments(count: usize) -> Vec<MultiSttSegment> {
        (0..count)
            .map(|i| MultiSttSegment {
                segment_id: i + 1,
                start: i as f64 * 2.0,
                end: (i as f64 + 1.0) * 2.0,
                speaker: format!("Speaker {}", (i % 2) + 1),
                primary_text: format!("segment {i} primary text"),
                tr_backup_text: format!("segment {i} backup text"),
                fast_helper_text: format!("segment {i} helper text"),
            })
            .collect()
    }

    fn config_for(url: String) -> PostCorrectionConfig {
        PostCorrectionConfig {
            enabled: true,
            base_url: url,
            api_key: "sk-test-secret-key-987".into(),
            primary_judge_model: "Qwen3.8-27B".into(),
            support_critic_models: vec!["gemma-4-31B-it".into()],
            segments_per_batch: 5,
            request_timeout_sec: 10,
            max_concurrent_batches: 2,
            continue_on_critic_failure: true,
        }
    }

    #[test]
    fn stage1_and_stage2_full_json_roundtrip_and_audit_redaction() {
        let server = StubServer::start(|req| {
            let body = req.json();
            let model = body["model"].as_str().unwrap_or("");
            match model {
                "gemma-4-31B-it" => chat_ok(
                    r#"{"candidates":[{"segment_id":2,"wrong_in_primary":"LocalBOT'u","suggested_fix":"LokalBot'u","supported_by":"whisper-turbo-tr-khan","reason":"proper noun"}]}"#,
                ),
                "gpt-oss-120b" => chat_ok(
                    "<|start|>assistant<|channel|>analysis<|message|>checking segment 1<|end|><|start|>assistant<|channel|>final<|constrain|>json<|message|>{\"candidates\":[{\"segment_id\":1,\"wrong_in_primary\":\"videolardan\",\"suggested_fix\":\"üzerinden\",\"supported_by\":\"zipformer-oayk\",\"reason\":\"phonetic\"}]}<|return|>",
                ),
                "Qwen3.8-27B" => chat_ok(
                    "<think>verify 1-to-1 segments</think>\n{\"segments\":[{\"segment_id\":1,\"text\":\"sistem üzerinden kurulur\"},{\"segment_id\":2,\"text\":\"LokalBot'u açın\"}]}",
                ),
                _ => Reply::Text(404, "unknown model".into()),
            }
        });

        let segments = vec![
            MultiSttSegment {
                segment_id: 1,
                start: 0.0,
                end: 2.5,
                speaker: "Speaker 1".into(),
                primary_text: "sistem videolardan kurulur".into(),
                tr_backup_text: "sistem üzerinden kurulur".into(),
                fast_helper_text: "sistem üzerinden kurulur".into(),
            },
            MultiSttSegment {
                segment_id: 2,
                start: 2.5,
                end: 5.0,
                speaker: "Speaker 2".into(),
                primary_text: "LocalBOT'u açın".into(),
                tr_backup_text: "LokalBot'u açın".into(),
                fast_helper_text: "lokalbotu açın".into(),
            },
        ];

        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gemma-4-31B-it".into(), "gpt-oss-120b".into()];
        assert!(!format!("{cfg:?}").contains("sk-test-secret-key-987"));

        let corrector = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .with_vocabulary(vec!["LokalBot".into()]);
        let out = corrector.correct(&segments).unwrap();

        assert_eq!(out.texts, ["sistem üzerinden kurulur", "LokalBot'u açın"]);
        assert_eq!(out.audit.schema_version, 1);
        assert_eq!(out.audit.segments.len(), 2);
        assert!(out.audit.segments[0].changed);
        assert!(!out.audit.segments[0].judge_fallback);
        let audit_json = serde_json::to_string_pretty(&out.audit).unwrap();
        assert!(!audit_json.contains("sk-test-secret-key-987"));

        let reqs = server.requests();
        assert!(reqs.iter().any(|r| {
            r.header("authorization").as_deref() == Some("Bearer sk-test-secret-key-987")
        }));
        let judge_req = reqs
            .iter()
            .find(|r| r.json()["model"] == "Qwen3.8-27B")
            .unwrap()
            .json();
        let judge_prompt = judge_req["messages"][1]["content"].as_str().unwrap();
        assert!(judge_prompt.contains("LokalBot"));
        assert!(judge_prompt.contains("gemma-4-31B-it"));
        assert!(judge_prompt.contains("gpt-oss-120b"));
        assert!(judge_prompt.contains("primary (whisper-turbo)"));
        assert!(judge_prompt.contains("tr_backup (whisper-turbo-tr-khan)"));
        assert!(judge_prompt.contains("fast_helper (zipformer-oayk)"));
    }

    #[test]
    fn critic_response_format_rejection_falls_back_once_per_model() {
        let gemma_calls = Arc::new(AtomicUsize::new(0));
        let gemma_calls_srv = gemma_calls.clone();
        let server = StubServer::start(move |req| {
            let body = req.json();
            let model = body["model"].as_str().unwrap_or("");
            if model == "gemma-4-31B-it" {
                gemma_calls_srv.fetch_add(1, Ordering::SeqCst);
                if body.get("response_format").is_some() {
                    return Reply::Json(
                        400,
                        json!({"error": {"message": "response_format json_schema is unsupported"}}),
                    );
                }
                return chat_ok(r#"{"candidates":[]}"#);
            }
            chat_ok(r#"{"segments":[{"segment_id":1,"text":"hello world"}]}"#)
        });

        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gemma-4-31B-it".into()];
        let segs = vec![MultiSttSegment {
            segment_id: 1,
            start: 0.0,
            end: 1.0,
            speaker: "S1".into(),
            primary_text: "hello world".into(),
            tr_backup_text: "hello world".into(),
            fast_helper_text: "hello world".into(),
        }];
        let corrector = PostCorrector::new(cfg, Vec::new()).unwrap();
        let out = corrector.correct(&segs).unwrap();
        assert_eq!(gemma_calls.load(Ordering::SeqCst), 2);
        assert!(out.audit.segments[0].critic_candidates[0].response_format_fallback);
        assert_eq!(out.texts, ["hello world"]);
    }

    #[test]
    fn critic_line_diff_fallback_parses_candidates() {
        let server = StubServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "gpt-oss-120b" => chat_ok(
                    "<think>analysis</think>\n- [Seg 1] | videolardan => üzerinden | zipformer-oayk | phonetic\nmalformed ignored line\n2 | localbot => LokalBot | whisper-turbo-tr-khan\n999 | ignored => unknown | zipformer-oayk",
                ),
                "Qwen3.8-27B" => chat_ok(
                    r#"{"segments":[{"segment_id":1,"text":"üzerinden"},{"segment_id":2,"text":"LokalBot"}]}"#,
                ),
                _ => Reply::Text(404, "nope".into()),
            }
        });

        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gpt-oss-120b".into()];
        let segs = sample_segments(2);
        let corrector = PostCorrector::new(cfg, Vec::new()).unwrap();
        let out = corrector.correct(&segs).unwrap();
        assert_eq!(out.texts, ["üzerinden", "LokalBot"]);
        assert!(out.audit.segments[0].critic_candidates[0].used_line_diff_fallback);
        assert_eq!(
            out.audit.segments[0].critic_candidates[0].candidates[0].suggested_fix,
            "üzerinden"
        );
        assert_eq!(
            out.audit.segments[1].critic_candidates[0].candidates[0].suggested_fix,
            "LokalBot"
        );
    }

    #[test]
    fn batching_neighbor_context_and_concurrency_limit_are_enforced() {
        let active_judges = Arc::new(AtomicUsize::new(0));
        let max_judges = Arc::new(AtomicUsize::new(0));
        let active_ref = active_judges.clone();
        let max_ref = max_judges.clone();

        let server = StubServer::start(move |req| {
            let body = req.json();
            let model = body["model"].as_str().unwrap_or("");
            if model == "Qwen3.8-27B" {
                let now = active_ref.fetch_add(1, Ordering::SeqCst) + 1;
                max_ref.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(80));
                active_ref.fetch_sub(1, Ordering::SeqCst);
                let prompt = body["messages"][1]["content"].as_str().unwrap();
                let mut ids = Vec::new();
                for line in prompt.lines() {
                    if let Some(rest) = line.strip_prefix("[Seg ")
                        && let Some((id_str, _)) = rest.split_once(']')
                        && !prompt
                            .split("Target segments:")
                            .nth(1)
                            .unwrap_or("")
                            .split("Next context")
                            .next()
                            .unwrap_or("")
                            .is_empty()
                    {
                        let _ = id_str;
                    }
                }
                let target_section = prompt
                    .split("Target segments:\n")
                    .nth(1)
                    .unwrap_or("")
                    .split("\nNext context")
                    .next()
                    .unwrap_or("");
                for line in target_section.lines() {
                    if let Some(rest) = line.strip_prefix("[Seg ")
                        && let Some((id_str, _)) = rest.split_once(']')
                        && let Ok(id) = id_str.parse::<usize>()
                    {
                        ids.push(json!({"segment_id": id, "text": format!("fixed {id}")}));
                    }
                }
                return chat_ok(&json!({"segments": ids}).to_string());
            }
            chat_ok(r#"{"candidates":[]}"#)
        });

        let mut cfg = config_for(server.url("/v1"));
        cfg.segments_per_batch = 5;
        cfg.max_concurrent_batches = 2;
        cfg.support_critic_models = vec!["gemma-4-31B-it".into()];

        let segs = sample_segments(12);
        let corrector = PostCorrector::new(cfg, Vec::new()).unwrap();
        let out = corrector.correct(&segs).unwrap();

        assert_eq!(out.texts.len(), 12);
        assert_eq!(out.texts[0], "fixed 1");
        assert_eq!(out.texts[11], "fixed 12");
        assert!(max_judges.load(Ordering::SeqCst) <= 2);

        let judge_prompts: Vec<String> = server
            .requests()
            .into_iter()
            .filter(|r| r.json()["model"] == "Qwen3.8-27B")
            .map(|r| {
                r.json()["messages"][1]["content"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(judge_prompts.len(), 3);
        assert!(
            judge_prompts
                .iter()
                .any(|p| p.contains("Previous context (read-only):")
                    && p.contains("Next context (read-only):"))
        );
    }

    #[test]
    fn continue_on_critic_failure_controls_partial_and_total_failures() {
        let server = StubServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "gpt-oss-120b" => Reply::Text(500, "gateway error".into()),
                "gemma-4-31B-it" => chat_ok(
                    r#"{"candidates":[{"segment_id":1,"wrong_in_primary":"primary","suggested_fix":"corrected","supported_by":"whisper-turbo-tr-khan","reason":"fix"}]}"#,
                ),
                "Qwen3.8-27B" => {
                    chat_ok(r#"{"segments":[{"segment_id":1,"text":"segment 0 corrected text"}]}"#)
                }
                _ => Reply::Text(404, "nope".into()),
            }
        });

        let segs = sample_segments(1);
        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gemma-4-31B-it".into(), "gpt-oss-120b".into()];
        cfg.continue_on_critic_failure = true;
        let out = PostCorrector::new(cfg.clone(), Vec::new())
            .unwrap()
            .correct(&segs)
            .unwrap();
        assert_eq!(out.texts, ["segment 0 corrected text"]);
        assert!(!out.audit.warnings.is_empty());

        cfg.continue_on_critic_failure = false;
        let err = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&segs)
            .unwrap_err();
        let err_str = format!("{err:#}");
        assert!(err_str.contains("gpt-oss-120b"));
        assert!(!err_str.contains("sk-test-secret-key-987"));
    }

    #[test]
    fn judge_segment_mismatch_falls_back_to_whisper_turbo_backbone() {
        let server = StubServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "Qwen3.8-27B" => chat_ok(r#"{"segments":[{"segment_id":1,"text":"only one"}]}"#),
                _ => chat_ok(r#"{"candidates":[]}"#),
            }
        });

        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gemma-4-31B-it".into()];
        let segs = sample_segments(2);
        let out = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&segs)
            .unwrap();
        assert_eq!(
            out.texts,
            [segs[0].primary_text.clone(), segs[1].primary_text.clone()]
        );
        assert!(
            out.audit
                .segments
                .iter()
                .all(|s| s.judge_fallback && !s.changed)
        );
        assert!(!out.audit.warnings.is_empty());
    }

    #[test]
    fn request_timeout_cancels_stalled_critic_gracefully() {
        let server = StubServer::start(|req| {
            let body = req.json();
            if body["model"] == "gpt-oss-120b" {
                std::thread::sleep(Duration::from_secs(3));
                return chat_ok(r#"{"candidates":[]}"#);
            }
            chat_ok(r#"{"segments":[{"segment_id":1,"text":"segment 0 primary text"}]}"#)
        });

        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gpt-oss-120b".into()];
        cfg.request_timeout_sec = 1;
        cfg.continue_on_critic_failure = true;
        let segs = sample_segments(1);
        let out = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&segs)
            .unwrap();
        assert_eq!(out.texts, [segs[0].primary_text.clone()]);
        assert!(out.audit.segments[0].critic_candidates[0].error.is_some());
    }

    #[test]
    fn consent_and_secret_redaction_are_enforced() {
        let mut cfg = config_for("https://remote-gateway.example.com/v1".into());
        let err = PostCorrector::new(cfg.clone(), Vec::new()).err().unwrap();
        assert!(matches!(
            err.downcast_ref::<AiError>(),
            Some(AiError::ConsentRequired(_))
        ));

        cfg.base_url = "http://remote-gateway.example.com/v1".into();
        let err = PostCorrector::new(
            cfg.clone(),
            vec!["http://remote-gateway.example.com".into()],
        )
        .err()
        .unwrap();
        assert!(matches!(
            err.downcast_ref::<AiError>(),
            Some(AiError::ConsentRequired(_))
        ));

        cfg.base_url = "https://remote-gateway.example.com/v1".into();
        let corrector =
            PostCorrector::new(cfg, vec!["https://remote-gateway.example.com".into()]).unwrap();
        let redacted = corrector.redact("failed with Bearer sk-test-secret-key-987 on request");
        assert!(!redacted.contains("sk-test-secret-key-987"));
    }

    #[test]
    fn api_key_precedence_prefers_config_over_keyring() {
        let server =
            StubServer::start(|_| chat_ok(r#"{"segments":[{"segment_id":1,"text":"ok"}]}"#));
        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = Vec::new();
        cfg.api_key = "sk-from-config".into();
        let segs = sample_segments(1);

        let corrector = PostCorrector::new(cfg.clone(), Vec::new())
            .unwrap()
            .with_keyring_fallback(Some("sk-from-keyring".into()));
        corrector.correct(&segs).unwrap();
        assert_eq!(
            server
                .requests()
                .pop()
                .unwrap()
                .header("authorization")
                .as_deref(),
            Some("Bearer sk-from-config")
        );

        cfg.api_key = "   ".into();
        let fallback = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .with_keyring_fallback(Some("sk-from-keyring".into()));
        fallback.correct(&segs).unwrap();
        assert_eq!(
            server
                .requests()
                .pop()
                .unwrap()
                .header("authorization")
                .as_deref(),
            Some("Bearer sk-from-keyring")
        );
    }

    #[test]
    fn critic_line_diff_handles_bracket_colon_quotes_noop_and_out_of_batch() {
        let allowed = HashSet::from([1usize, 2usize, 3usize]);
        let (quoted, used_ld) = critic::parse_critic_reply(
            "- [Seg 1]: | \"foo\" => \"bar\" | zipformer-oayk | quoted\n[Segment #2]: | 'baz' => 'qux' | context | single quoted\n3 | LocalBOT'u => LokalBot'u | context | inner apostrophe",
            &allowed,
        )
        .unwrap();
        assert!(used_ld);
        assert_eq!(quoted.len(), 3);
        assert_eq!(quoted[0].segment_id, 1);
        assert_eq!(quoted[0].wrong_in_primary, "foo");
        assert_eq!(quoted[0].suggested_fix, "bar");
        assert_eq!(quoted[1].segment_id, 2);
        assert_eq!(quoted[1].wrong_in_primary, "baz");
        assert_eq!(quoted[1].suggested_fix, "qux");
        assert_eq!(quoted[2].segment_id, 3);
        assert_eq!(quoted[2].wrong_in_primary, "LocalBOT'u");
        assert_eq!(quoted[2].suggested_fix, "LokalBot'u");

        let (out_of_batch, out_ld) = critic::parse_critic_reply(
            "- [Seg 9999] | wrong => fix | zipformer-oayk | out of batch\n",
            &allowed,
        )
        .unwrap();
        assert!(out_ld);
        assert!(out_of_batch.is_empty());

        let (noop, noop_ld) = critic::parse_critic_reply(
            "- [Seg 1] | same => same | zipformer-oayk | noop\n",
            &allowed,
        )
        .unwrap();
        assert!(noop_ld);
        assert!(noop.is_empty());

        assert!(critic::parse_critic_reply("Cannot comply with your request", &allowed).is_err());

        let server = StubServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "gpt-oss-120b" => chat_ok(
                    "- [Seg 9999] | wrong => fix | zipformer-oayk | out of batch\n- [Seg 1]: | \"primary\" => \"fixed\" | zipformer-oayk | quote",
                ),
                "Qwen3.8-27B" => {
                    chat_ok(r#"{"segments":[{"segment_id":1,"text":"segment 0 fixed text"}]}"#)
                }
                _ => Reply::Text(404, "nope".into()),
            }
        });
        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gpt-oss-120b".into()];
        cfg.continue_on_critic_failure = false;
        let out = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&sample_segments(1))
            .unwrap();
        assert_eq!(out.texts, ["segment 0 fixed text"]);
        assert!(out.audit.segments[0].critic_candidates[0].used_line_diff_fallback);
        assert!(
            out.audit.segments[0].critic_candidates[0].candidates[0].wrong_in_primary == "primary"
        );
        assert!(out.audit.segments[0].critic_candidates[0].candidates[0].suggested_fix == "fixed");
    }

    #[test]
    fn multi_batch_abort_returns_actual_batch_error_instead_of_missing_slot() {
        let server = StubServer::start(|req| {
            let body = req.json();
            let prompt = body["messages"][1]["content"].as_str().unwrap_or("");
            if prompt.contains("[Seg 1]") {
                std::thread::sleep(Duration::from_millis(250));
                return chat_ok(r#"{"segments":[{"segment_id":1,"text":"ok"}]}"#);
            }
            Reply::Text(
                500,
                "batch 2 exploded with Bearer sk-test-secret-key-987".into(),
            )
        });
        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["gpt-oss-120b".into()];
        cfg.continue_on_critic_failure = false;
        cfg.segments_per_batch = 1;
        cfg.max_concurrent_batches = 2;

        let err = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&sample_segments(3))
            .unwrap_err();
        let err_str = format!("{err:#}");
        assert!(
            !err_str.contains("missing batch result"),
            "unexpected error: {err_str}"
        );
        assert!(!err_str.contains("sk-test-secret-key-987"));
    }

    #[test]
    fn local_catalog_models_do_not_require_remote_endpoint() {
        assert!(is_local_catalog_model("qwen3.5-4b"));
        assert!(!is_local_catalog_model("Qwen3.8-27B"));
        let mut cfg = PostCorrectionConfig {
            enabled: true,
            base_url: String::new(),
            api_key: String::new(),
            primary_judge_model: "qwen3.5-4b".into(),
            support_critic_models: vec!["qwen3.5-4b".into()],
            ..Default::default()
        };
        assert!(!requires_remote_endpoint(&cfg));
        assert!(PostCorrector::new(cfg.clone(), Vec::new()).is_ok());

        cfg.support_critic_models.push("gpt-oss-120b".into());
        assert!(requires_remote_endpoint(&cfg));
        assert!(PostCorrector::new(cfg, Vec::new()).is_err());
    }

    #[test]
    fn critic_is_skipped_after_three_failures() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let critic_calls = Arc::new(AtomicUsize::new(0));
        let critic_calls_srv = Arc::clone(&critic_calls);
        let server = StubServer::start(move |req| {
            let body = req.json();
            let model = body["model"].as_str().unwrap_or("");
            if model == "bad-critic" {
                critic_calls_srv.fetch_add(1, Ordering::SeqCst);
                chat_ok("not valid json or line-diff")
            } else {
                let ids: Vec<serde_json::Value> = (0..5)
                    .filter(|id| {
                        body["messages"][1]["content"]
                            .as_str()
                            .unwrap_or("")
                            .contains(&format!("[Seg {id}]"))
                    })
                    .map(|id| serde_json::json!({ "segment_id": id, "text": format!("ok {id}") }))
                    .collect();
                chat_ok(&serde_json::json!({ "segments": ids }).to_string())
            }
        });
        let mut cfg = config_for(server.url("/v1"));
        cfg.support_critic_models = vec!["bad-critic".into()];
        cfg.continue_on_critic_failure = true;
        cfg.segments_per_batch = 1;
        cfg.max_concurrent_batches = 1;

        let out = PostCorrector::new(cfg, Vec::new())
            .unwrap()
            .correct(&sample_segments(5))
            .unwrap();
        assert_eq!(out.texts.len(), 5);
        assert_eq!(critic_calls.load(Ordering::SeqCst), 3);
        assert!(
            out.audit
                .warnings
                .iter()
                .any(|w| w.contains("skipping it for remaining batches"))
        );
    }
}
