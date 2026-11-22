use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AiError;
use crate::json::{complete_records, extract_json_object, strip_code_fence, strip_reasoning};
use crate::think::ChatRequest;

use super::{CriticModelResult, MultiSttSegment, PostCorrector};

const JUDGE_SYSTEM_PROMPT: &str = "\
You are Stage 2 (Main Judge) of an asymmetric multi-STT post-correction system.
For each target segment you receive:
- primary (whisper-turbo)
- tr_backup (whisper-turbo-tr-khan)
- fast_helper (zipformer-oayk)
- diff candidate suggestions from parallel Stage 1 critics.

MANDATORY RULES:
1. Backbone Preservation Rule: primary (whisper-turbo) is the default truth. Do NOT paraphrase, rewrite sentence structure, translate, or alter the speaker's natural phrasing or valid English/technical terms.
2. Evidence-Only Edits: Only replace a word, suffix, or proper name when acoustic/phonetic evidence from tr_backup, fast_helper, critic candidates, or vocabulary clearly proves an error in primary.
3. 1-to-1 Segment Mapping Rule: Return EXACTLY one entry per input target segment_id in the same order, with identical segment_id values. Never merge, split, drop, or invent segments.
4. Return strict JSON:
   {\"segments\": [{\"segment_id\": <int>, \"text\": \"<final corrected text>\"}]}";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JudgeSegment {
    pub segment_id: usize,
    pub text: String,
}

pub(crate) struct JudgeBatchResult {
    pub texts: Vec<String>,
    pub judge_fallback: bool,
    pub warning: Option<String>,
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["segments"],
        "properties": {
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["segment_id", "text"],
                    "properties": {
                        "segment_id": { "type": "integer" },
                        "text": { "type": "string" }
                    }
                }
            }
        }
    })
}

pub(crate) fn build_request(
    batch: &[MultiSttSegment],
    prev_context: &[MultiSttSegment],
    next_context: &[MultiSttSegment],
    vocabulary: &[String],
    critic_results: &[CriticModelResult],
) -> ChatRequest {
    let mut user = String::new();
    if !vocabulary.is_empty() {
        user.push_str("Vocabulary (domain terms and proper names):\n");
        user.push_str(&vocabulary.join(", "));
        user.push_str("\n\n");
    }
    if !prev_context.is_empty() {
        user.push_str("Previous context (read-only):\n");
        for seg in prev_context {
            user.push_str(&format!(
                "[Seg {}] ({}): {}\n",
                seg.segment_id, seg.speaker, seg.primary_text
            ));
        }
        user.push('\n');
    }
    user.push_str("Target segments:\n");
    for seg in batch {
        user.push_str(&format!(
            "[Seg {}] ({}, {:.2}s-{:.2}s)\n  primary (whisper-turbo): {}\n  tr_backup (whisper-turbo-tr-khan): {}\n  fast_helper (zipformer-oayk): {}\n",
            seg.segment_id,
            seg.speaker,
            seg.start,
            seg.end,
            seg.primary_text,
            seg.tr_backup_text,
            seg.fast_helper_text,
        ));
        let mut any_suggestion = false;
        for critic in critic_results {
            for cand in critic
                .candidates
                .iter()
                .filter(|c| c.segment_id == seg.segment_id)
            {
                if !any_suggestion {
                    user.push_str("  critic suggestions:\n");
                    any_suggestion = true;
                }
                user.push_str(&format!(
                    "    - [{}] \"{}\" => \"{}\" (supported_by: {}, reason: {})\n",
                    critic.model,
                    cand.wrong_in_primary,
                    cand.suggested_fix,
                    cand.supported_by,
                    cand.reason
                ));
            }
        }
    }
    if !next_context.is_empty() {
        user.push_str("\nNext context (read-only):\n");
        for seg in next_context {
            user.push_str(&format!(
                "[Seg {}] ({}): {}\n",
                seg.segment_id, seg.speaker, seg.primary_text
            ));
        }
    }
    ChatRequest::new(JUDGE_SYSTEM_PROMPT, user)
        .temperature(0.0)
        .reasoning_budget(0)
        .json_schema(schema())
}

pub(crate) fn validate_judge_segments(
    batch: &[MultiSttSegment],
    parsed: &[JudgeSegment],
) -> Result<Vec<String>> {
    if parsed.len() != batch.len() {
        return Err(AiError::BadResponse(format!(
            "judge returned {} segments for a batch of {}",
            parsed.len(),
            batch.len()
        ))
        .into());
    }
    let expected_ids: HashSet<usize> = batch.iter().map(|s| s.segment_id).collect();
    let mut by_id = HashMap::with_capacity(batch.len());
    for item in parsed {
        if !expected_ids.contains(&item.segment_id) {
            return Err(AiError::BadResponse(format!(
                "judge returned unexpected segment_id {}",
                item.segment_id
            ))
            .into());
        }
        if by_id.insert(item.segment_id, item.text.clone()).is_some() {
            return Err(AiError::BadResponse(format!(
                "judge returned duplicate segment_id {}",
                item.segment_id
            ))
            .into());
        }
    }
    let mut texts = Vec::with_capacity(batch.len());
    for seg in batch {
        let Some(text) = by_id.remove(&seg.segment_id) else {
            return Err(AiError::BadResponse(format!(
                "judge omitted segment_id {}",
                seg.segment_id
            ))
            .into());
        };
        if !seg.primary_text.trim().is_empty() && text.trim().is_empty() {
            return Err(AiError::BadResponse(format!(
                "judge returned empty text for non-empty segment_id {}",
                seg.segment_id
            ))
            .into());
        }
        texts.push(text);
    }
    Ok(texts)
}

fn parse_judge_reply(raw: &str) -> Result<Vec<JudgeSegment>> {
    let cleaned = strip_reasoning(raw);
    let unfenced = strip_code_fence(&cleaned);

    if let Some(json_str) = extract_json_object(unfenced).or_else(|| extract_json_object(&cleaned))
    {
        #[derive(Deserialize)]
        struct Envelope {
            segments: Vec<JudgeSegment>,
        }
        if let Ok(env) = serde_json::from_str::<Envelope>(json_str) {
            return Ok(env.segments);
        }
    }

    let recovered = complete_records(unfenced, &["segments"]);
    if let Some(records) = recovered.arrays.get("segments")
        && !records.is_empty()
    {
        let mut parsed = Vec::with_capacity(records.len());
        for obj in records {
            let segment_id = obj
                .get("segment_id")
                .and_then(|v| {
                    v.as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<usize>().ok()))
                })
                .context("missing segment_id in recovered judge record")?;
            let text = obj
                .get("text")
                .and_then(Value::as_str)
                .context("missing text in recovered judge record")?
                .to_string();
            parsed.push(JudgeSegment { segment_id, text });
        }
        return Ok(parsed);
    }

    Err(AiError::BadResponse("judge reply did not contain valid segments JSON".into()).into())
}

pub(crate) fn run_batch(
    corrector: &PostCorrector,
    batch: &[MultiSttSegment],
    prev_context: &[MultiSttSegment],
    next_context: &[MultiSttSegment],
    critic_results: &[CriticModelResult],
) -> Result<JudgeBatchResult> {
    let request = build_request(
        batch,
        prev_context,
        next_context,
        &corrector.vocabulary,
        critic_results,
    );
    let call = corrector.complete_json_or_text(&corrector.config.primary_judge_model, &request)?;
    match parse_judge_reply(&call.text).and_then(|parsed| validate_judge_segments(batch, &parsed))
    {
        Ok(texts) => Ok(JudgeBatchResult {
            texts,
            judge_fallback: false,
            warning: None,
        }),
        Err(err) => {
            let warning = corrector.redact(&format!(
                "judge batch fallback to whisper-turbo backbone: {err:#}"
            ));
            log::warn!("{warning}");
            Ok(JudgeBatchResult {
                texts: batch.iter().map(|s| s.primary_text.clone()).collect(),
                judge_fallback: true,
                warning: Some(warning),
            })
        }
    }
}
