use std::collections::HashSet;

use anyhow::Result;
use serde_json::{Value, json};

use crate::error::AiError;
use crate::json::{complete_records, extract_json_object, strip_code_fence, strip_reasoning};
use crate::think::ChatRequest;

use super::{CorrectionCandidate, CriticModelResult, MultiSttSegment, PostCorrector};

const CRITIC_SYSTEM_PROMPT: &str = "\
You are Stage 1 of a two-stage ASR post-correction system. You compare three raw speech-to-text transcripts for each segment:
- primary (whisper-turbo): default backbone; usually best on punctuation and English/technical terms.
- tr_backup (whisper-turbo-tr-khan): Turkish fine-tuned model; often catches Turkish words and suffixes missed by primary.
- fast_helper (zipformer-oayk): Zipformer2 acoustic model; often catches Turkish morphology and words missed by both Whisper models.

RULES:
1. NEVER rewrite, paraphrase, or output the full transcript text.
2. ONLY report short spans in primary that appear acoustically or phonetically wrong when compared against tr_backup, fast_helper, context, and vocabulary.
3. Return strict JSON:
   {\"candidates\": [{\"segment_id\": <int>, \"wrong_in_primary\": \"<exact span>\", \"suggested_fix\": \"<replacement>\", \"supported_by\": \"<whisper-turbo-tr-khan|zipformer-oayk|context>\", \"reason\": \"<brief reason>\"}]}
4. If primary has no clear errors, return {\"candidates\": []}.
5. If JSON output is not possible, return one line per candidate:
   SEG_ID | wrong_in_primary => suggested_fix | supported_by | reason";

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["candidates"],
        "properties": {
            "candidates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "segment_id",
                        "wrong_in_primary",
                        "suggested_fix",
                        "supported_by",
                        "reason"
                    ],
                    "properties": {
                        "segment_id": { "type": "integer" },
                        "wrong_in_primary": { "type": "string" },
                        "suggested_fix": { "type": "string" },
                        "supported_by": { "type": "string" },
                        "reason": { "type": "string" }
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
    ChatRequest::new(CRITIC_SYSTEM_PROMPT, user)
        .temperature(0.0)
        .reasoning_budget(0)
        .json_schema(schema())
}

pub(crate) fn run_batch(
    corrector: &PostCorrector,
    model: &str,
    batch: &[MultiSttSegment],
    prev_context: &[MultiSttSegment],
    next_context: &[MultiSttSegment],
) -> CriticModelResult {
    let allowed_ids: HashSet<usize> = batch.iter().map(|s| s.segment_id).collect();
    let request = build_request(batch, prev_context, next_context, &corrector.vocabulary);
    match corrector.complete_json_or_text(model, &request) {
        Ok(call) => match parse_critic_reply(&call.text, &allowed_ids) {
            Ok((candidates, used_line_diff_fallback)) => CriticModelResult {
                model: model.to_string(),
                candidates,
                used_line_diff_fallback,
                response_format_fallback: call.response_format_fallback,
                error: None,
            },
            Err(err) => CriticModelResult {
                model: model.to_string(),
                candidates: Vec::new(),
                used_line_diff_fallback: false,
                response_format_fallback: call.response_format_fallback,
                error: Some(corrector.redact(&format!("{err:#}"))),
            },
        },
        Err(err) => CriticModelResult {
            model: model.to_string(),
            candidates: Vec::new(),
            used_line_diff_fallback: false,
            response_format_fallback: false,
            error: Some(corrector.redact(&format!("{err:#}"))),
        },
    }
}

fn parse_segment_id_value(value: &Value) -> Option<usize> {
    if let Some(id) = value.as_u64() {
        return usize::try_from(id).ok();
    }
    value.as_str().and_then(parse_segment_id_str)
}

fn parse_segment_id_str(raw: &str) -> Option<usize> {
    let trimmed = raw.trim().trim_matches(['[', ']', ':', ' ', '\t']).trim();
    let lower = trimmed.to_ascii_lowercase();
    let digits = if let Some(rest) = lower.strip_prefix("segment") {
        rest
    } else if let Some(rest) = lower.strip_prefix("seg") {
        rest
    } else {
        lower.as_str()
    };
    digits
        .trim()
        .trim_start_matches(['#', ':'])
        .trim()
        .parse::<usize>()
        .ok()
}

fn candidate_from_map(
    map: &serde_json::Map<String, Value>,
    allowed_ids: &HashSet<usize>,
) -> Option<CorrectionCandidate> {
    let segment_id = map.get("segment_id").and_then(parse_segment_id_value)?;
    if !allowed_ids.contains(&segment_id) {
        return None;
    }
    let wrong_in_primary = map
        .get("wrong_in_primary")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    let suggested_fix = map
        .get("suggested_fix")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if wrong_in_primary.is_empty() || suggested_fix.is_empty() || wrong_in_primary == suggested_fix
    {
        return None;
    }
    let supported_by = map
        .get("supported_by")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let reason = map
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Some(CorrectionCandidate {
        segment_id,
        wrong_in_primary,
        suggested_fix,
        supported_by,
        reason,
    })
}

struct LineDiffParse {
    candidates: Vec<CorrectionCandidate>,
    matched_any_syntax: bool,
}

fn strip_matching_quotes(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        && !inner.contains('"')
    {
        return inner.trim();
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        && !inner.contains('\'')
    {
        return inner.trim();
    }
    trimmed
}

fn parse_line_diff(text: &str, allowed_ids: &HashSet<usize>) -> LineDiffParse {
    let mut candidates = Vec::new();
    let mut matched_any_syntax = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let trimmed = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .unwrap_or(trimmed)
            .trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('|').collect();
        if parts.len() < 2 {
            continue;
        }
        let Some(segment_id) = parse_segment_id_str(parts[0]) else {
            continue;
        };
        let diff_part = parts[1].trim();
        let Some((wrong_raw, fix_raw)) = diff_part
            .split_once("=>")
            .or_else(|| diff_part.split_once("->"))
        else {
            continue;
        };
        let wrong_in_primary = strip_matching_quotes(wrong_raw).to_string();
        let suggested_fix = strip_matching_quotes(fix_raw).to_string();
        if wrong_in_primary.is_empty() || suggested_fix.is_empty() {
            continue;
        }
        matched_any_syntax = true;
        if !allowed_ids.contains(&segment_id) || wrong_in_primary == suggested_fix {
            continue;
        }
        let supported_by = parts
            .get(2)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let reason = parts
            .get(3)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        candidates.push(CorrectionCandidate {
            segment_id,
            wrong_in_primary,
            suggested_fix,
            supported_by,
            reason,
        });
    }
    LineDiffParse {
        candidates,
        matched_any_syntax,
    }
}

pub(crate) fn parse_critic_reply(
    raw: &str,
    allowed_ids: &HashSet<usize>,
) -> Result<(Vec<CorrectionCandidate>, bool)> {
    let cleaned = strip_reasoning(raw);
    let unfenced = strip_code_fence(&cleaned);

    if let Some(json_str) = extract_json_object(unfenced).or_else(|| extract_json_object(&cleaned))
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json_str)
        && let Some(items) = map.get("candidates").and_then(Value::as_array)
    {
        let candidates = items
            .iter()
            .filter_map(|item| item.as_object())
            .filter_map(|obj| candidate_from_map(obj, allowed_ids))
            .collect();
        return Ok((candidates, false));
    }

    let recovered = complete_records(unfenced, &["candidates"]);
    if let Some(records) = recovered.arrays.get("candidates")
        && !records.is_empty()
    {
        let candidates = records
            .iter()
            .filter_map(|obj| candidate_from_map(obj, allowed_ids))
            .collect();
        return Ok((candidates, false));
    }

    let line_parse = parse_line_diff(unfenced, allowed_ids);
    if line_parse.matched_any_syntax {
        return Ok((line_parse.candidates, true));
    }

    let normalized = unfenced.trim().to_ascii_uppercase();
    if matches!(
        normalized.as_str(),
        "NONE" | "NO_CHANGES" | "NO CHANGES" | "[]" | "{\"CANDIDATES\":[]}"
    ) {
        return Ok((Vec::new(), false));
    }

    Err(AiError::BadResponse("critic reply was neither valid JSON nor line-diff".into()).into())
}
