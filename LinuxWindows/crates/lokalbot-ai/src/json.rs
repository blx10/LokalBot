use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

const HARMONY_ANALYSIS_OPENERS: [&str; 4] = [
    "<|start|>assistant<|channel|>analysis",
    "<|start|>assistant<|channel|>commentary",
    "<|channel|>analysis",
    "<|channel|>commentary",
];

const HARMONY_CONTROL_TOKENS: [&str; 13] = [
    "<|return|>",
    "<|end|>",
    "<|call|>",
    "<|start|>assistant",
    "<|start|>user",
    "<|start|>system",
    "<|start|>",
    "<|message|>",
    "<|channel|>final",
    "<|channel|>analysis",
    "<|channel|>commentary",
    "<|constrain|>json",
    "<|constrain|>",
];

fn balanced_object_at(text: &str, start: usize) -> Option<&str> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

fn valid_json_object_end(text: &str, start: usize) -> Option<usize> {
    let candidate = balanced_object_at(text, start)?;
    serde_json::from_str::<Value>(candidate)
        .ok()
        .map(|_| start + candidate.len())
}

fn skip_think_end(text: &str, start: usize) -> Option<usize> {
    if !text[start..].starts_with("<think>") {
        return None;
    }
    let mut cursor = start + "<think>".len();
    let mut depth = 1usize;
    while depth > 0 {
        let next_close = text[cursor..].find("</think>")?;
        let next_open = text[cursor..].find("<think>");
        if let Some(open_rel) = next_open
            && open_rel < next_close
        {
            depth += 1;
            cursor += open_rel + "<think>".len();
        } else {
            depth -= 1;
            cursor += next_close + "</think>".len();
        }
    }
    Some(cursor)
}

fn strip_think_blocks(text: &str) -> String {
    let mut result = text.to_string();
    let mut search_from = 0usize;
    while let Some(rel) = result[search_from..].find("<think>") {
        let open = search_from + rel;
        let Some(close) = skip_think_end(&result, open) else {
            break;
        };
        result.replace_range(open..close, "");
        search_from = open;
    }
    result
}

fn find_harmony_final_end(rest: &str) -> usize {
    let leading_ws = rest.len() - rest.trim_start().len();
    let mut pos = 0usize;
    let mut in_truncated_json = false;
    let mut in_string = false;
    let mut escaped = false;
    while pos < rest.len() {
        if in_truncated_json {
            if in_string {
                let ch = rest[pos..].chars().next().unwrap();
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                pos += ch.len_utf8();
                continue;
            }
            if ["<|return|>", "<|end|>", "<|start|>"]
                .into_iter()
                .any(|marker| rest[pos..].starts_with(marker))
            {
                return pos;
            }
            let ch = rest[pos..].chars().next().unwrap();
            if ch == '"' {
                in_string = true;
            }
            pos += ch.len_utf8();
            continue;
        }
        if rest[pos..].starts_with("<think>")
            && let Some(end) = skip_think_end(rest, pos)
        {
            pos = end;
            continue;
        }
        if rest.as_bytes()[pos] == b'{' {
            if let Some(end) = valid_json_object_end(rest, pos) {
                pos = end;
                continue;
            }
            if pos == leading_ws {
                in_truncated_json = true;
                pos += 1;
                continue;
            }
        }
        if ["<|return|>", "<|end|>", "<|start|>"]
            .into_iter()
            .any(|marker| rest[pos..].starts_with(marker))
        {
            return pos;
        }
        let ch = rest[pos..].chars().next().unwrap();
        pos += ch.len_utf8();
    }
    rest.len()
}

fn strip_harmony_control_tokens(text: &str) -> String {
    let leading_ws = text.len() - text.trim_start().len();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    let mut in_truncated_json = false;
    let mut in_string = false;
    let mut escaped = false;
    while pos < text.len() {
        if in_truncated_json {
            if in_string {
                let ch = text[pos..].chars().next().unwrap();
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                out.push(ch);
                pos += ch.len_utf8();
                continue;
            }
            if text[pos..].starts_with("<|constrain|>") {
                let after = &text[pos + "<|constrain|>".len()..];
                if let Some(msg_rel) = after.find("<|message|>") {
                    let between = &after[..msg_rel];
                    if !between.contains("<|") && !between.contains('\n') && between.len() <= 64 {
                        pos += "<|constrain|>".len() + msg_rel + "<|message|>".len();
                        continue;
                    }
                }
                pos += "<|constrain|>".len();
                continue;
            }
            if let Some(token) = HARMONY_CONTROL_TOKENS
                .into_iter()
                .find(|token| text[pos..].starts_with(token))
            {
                pos += token.len();
                continue;
            }
            let ch = text[pos..].chars().next().unwrap();
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
            pos += ch.len_utf8();
            continue;
        }
        if text.as_bytes()[pos] == b'{' {
            if let Some(end) = valid_json_object_end(text, pos) {
                out.push_str(&text[pos..end]);
                pos = end;
                continue;
            }
            if pos == leading_ws {
                in_truncated_json = true;
                out.push('{');
                pos += 1;
                continue;
            }
        }
        if text[pos..].starts_with("<|constrain|>") {
            let after = &text[pos + "<|constrain|>".len()..];
            if let Some(msg_rel) = after.find("<|message|>") {
                let between = &after[..msg_rel];
                if !between.contains("<|") && !between.contains('\n') && between.len() <= 64 {
                    pos += "<|constrain|>".len() + msg_rel + "<|message|>".len();
                    continue;
                }
            }
            pos += "<|constrain|>".len();
            continue;
        }
        if let Some(token) = HARMONY_CONTROL_TOKENS
            .into_iter()
            .find(|token| text[pos..].starts_with(token))
        {
            pos += token.len();
            continue;
        }
        let ch = text[pos..].chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }
    out
}

fn find_earliest_opener_outside_json(text: &str) -> Option<(usize, &'static str)> {
    let mut pos = 0usize;
    while pos < text.len() {
        if text.as_bytes()[pos] == b'{'
            && let Some(end) = valid_json_object_end(text, pos)
        {
            pos = end;
            continue;
        }
        if let Some(opener) = HARMONY_ANALYSIS_OPENERS
            .into_iter()
            .find(|opener| text[pos..].starts_with(opener))
        {
            return Some((pos, opener));
        }
        let ch = text[pos..].chars().next().unwrap();
        pos += ch.len_utf8();
    }
    None
}

fn strip_closed_harmony_blocks(text: &str) -> String {
    let mut result = text.to_string();
    while let Some((open, opener)) = find_earliest_opener_outside_json(&result) {
        let search_from = open + opener.len();
        let Some(close_rel) = result[search_from..].find("<|end|>") else {
            break;
        };
        let close = search_from + close_rel + "<|end|>".len();
        result.replace_range(open..close, "");
    }
    result
}

fn strip_harmony(text: &str) -> String {
    if !text.contains("<|") {
        return text.to_string();
    }
    if let Some(final_pos) = text.rfind("<|channel|>final") {
        let after_channel = &text[final_pos + "<|channel|>final".len()..];
        if let Some(msg_rel) = after_channel.find("<|message|>") {
            let content_start =
                final_pos + "<|channel|>final".len() + msg_rel + "<|message|>".len();
            let rest = &text[content_start..];
            let end_rel = find_harmony_final_end(rest);
            return strip_harmony_control_tokens(&rest[..end_rel])
                .trim()
                .to_string();
        }
    }
    let mut result = strip_closed_harmony_blocks(text);
    if let Some((open, _)) = find_earliest_opener_outside_json(&result) {
        result.truncate(open);
    }
    strip_harmony_control_tokens(&result).trim().to_string()
}

pub fn strip_reasoning(text: &str) -> String {
    let harmony_stripped = strip_harmony(text);
    strip_think_blocks(&harmony_stripped).trim().to_string()
}

pub fn visible_text_of_truncated(reply: &str) -> String {
    if let Some(final_pos) = reply.rfind("<|channel|>final") {
        let after_channel = &reply[final_pos + "<|channel|>final".len()..];
        if let Some(msg_rel) = after_channel.find("<|message|>") {
            let start = final_pos + "<|channel|>final".len() + msg_rel + "<|message|>".len();
            let rest = &reply[start..];
            let end_rel = find_harmony_final_end(rest);
            let control_stripped = strip_harmony_control_tokens(&rest[..end_rel]);
            let closed_stripped = strip_think_blocks(&control_stripped);
            let visible = match closed_stripped.find("<think>") {
                Some(unclosed) => &closed_stripped[..unclosed],
                None => closed_stripped.as_str(),
            };
            return visible.trim().to_string();
        }
    }
    let cleaned = strip_think_blocks(&strip_closed_harmony_blocks(reply));
    let earliest_hidden = [
        "<think>",
        "<|start|>assistant<|channel|>analysis",
        "<|start|>assistant<|channel|>commentary",
        "<|channel|>analysis",
        "<|channel|>commentary",
    ]
    .into_iter()
    .filter_map(|marker| cleaned.find(marker))
    .min();
    match earliest_hidden {
        Some(open) => strip_harmony_control_tokens(&cleaned[..open])
            .trim()
            .to_string(),
        None if cleaned.contains("<|") => strip_harmony_control_tokens(&cleaned).trim().to_string(),
        None if cleaned != reply => cleaned.trim().to_string(),
        None => reply.to_string(),
    }
}

pub fn extract_json_object(text: &str) -> Option<&str> {
    let mut first_balanced = None;
    let mut search_from = 0usize;
    let mut attempts = 0usize;
    while let Some(rel) = text[search_from..].find('{') {
        let start = search_from + rel;
        let Some(candidate) = balanced_object_at(text, start) else {
            break;
        };
        if first_balanced.is_none() {
            first_balanced = Some(candidate);
        }
        if serde_json::from_str::<Value>(candidate).is_ok() {
            return Some(candidate);
        }
        attempts += 1;
        if attempts >= 256 {
            break;
        }
        search_from = start + candidate.len();
    }
    first_balanced
}

pub fn parse_object(text: &str) -> Option<Map<String, Value>> {
    let cleaned = strip_reasoning(text);
    let json = extract_json_object(&cleaned)?;
    match serde_json::from_str::<Value>(json).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompleteRecords {
    pub arrays: BTreeMap<String, Vec<Map<String, Value>>>,
    pub complete: bool,
    pub malformed_records: usize,
}

pub fn complete_records(text: &str, keys: &[&str]) -> CompleteRecords {
    let keys: BTreeSet<&str> = keys.iter().copied().collect();
    let text = strip_reasoning(text);
    let text = text.trim();
    if text.len() > 262_144 {
        return CompleteRecords::default();
    }
    if let Some(json) = extract_json_object(text)
        && let Ok(Value::Object(object)) = serde_json::from_str::<Value>(json)
    {
        let mut result = CompleteRecords {
            complete: keys
                .iter()
                .all(|k| object.get(*k).is_some_and(Value::is_array)),
            ..Default::default()
        };
        for key in &keys {
            let values = object
                .get(*key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let records: Vec<Map<String, Value>> = values
                .iter()
                .filter_map(|v| v.as_object().cloned())
                .collect();
            result.malformed_records += values.len() - records.len();
            result.arrays.insert((*key).to_string(), records);
        }
        return result;
    }

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.first().map(|c| c.1) != Some('{') {
        return CompleteRecords::default();
    }
    let mut result = CompleteRecords::default();
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0usize;
    let mut pending_key: Option<String> = None;
    let mut array_key: Option<String> = None;
    let mut record_start: Option<usize> = None;
    for &(byte, ch) in &chars {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                if stack == ['{'] {
                    pending_key =
                        serde_json::from_str::<String>(&text[string_start..byte + 1]).ok();
                }
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                string_start = byte;
            }
            '{' | '[' => {
                if ch == '[' && stack == ['{'] {
                    array_key = pending_key.take().filter(|k| keys.contains(k.as_str()));
                }
                if ch == '{' && stack == ['{', '['] && array_key.is_some() {
                    record_start = Some(byte);
                }
                stack.push(ch);
            }
            '}' | ']' => {
                let opener = if ch == '}' { '{' } else { '[' };
                if stack.last() != Some(&opener) {
                    break;
                }
                if ch == '}' && stack == ['{', '[', '{'] {
                    if let (Some(start), Some(key)) = (record_start, array_key.as_ref()) {
                        match serde_json::from_str::<Value>(&text[start..byte + 1]) {
                            Ok(Value::Object(record)) => {
                                result.arrays.entry(key.clone()).or_default().push(record)
                            }
                            _ => result.malformed_records += 1,
                        }
                    }
                    record_start = None;
                }
                stack.pop();
                if ch == ']' && stack == ['{'] {
                    array_key = None;
                }
            }
            _ => {}
        }
    }
    result
}

pub fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
            let mut out = Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

pub fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = match rest.find('\n') {
        Some(newline) => &rest[newline + 1..],
        None => rest,
    };
    body.strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_blocks_are_removed() {
        assert_eq!(
            strip_reasoning("<think>hmm</think>\n Answer <think>x</think>!"),
            "Answer !"
        );
        assert_eq!(strip_reasoning("<think>unclosed"), "<think>unclosed");
        assert_eq!(visible_text_of_truncated("Hi <think>long"), "Hi");
    }

    #[test]
    fn extracts_balanced_objects_with_strings() {
        let text = r#"Sure: {"a": "brace } in \"string\"", "b": {"c": 1}} trailing {"x":1}"#;
        assert_eq!(
            extract_json_object(text),
            Some(r#"{"a": "brace } in \"string\"", "b": {"c": 1}}"#)
        );
        assert_eq!(extract_json_object("no json"), None);
        assert_eq!(extract_json_object("{\"open\": 1"), None);
    }

    #[test]
    fn complete_records_survive_truncation() {
        let full = r#"{"notes":[{"t":"a"},{"t":"b"}],"actions":[],"has_more":false}"#;
        let parsed = complete_records(full, &["notes", "actions"]);
        assert!(parsed.complete);
        assert_eq!(parsed.arrays["notes"].len(), 2);

        let cut = r#"{"notes":[{"t":"a, with } brace"},{"t":"b"},{"t":"unfinish"#;
        let parsed = complete_records(cut, &["notes", "actions"]);
        assert!(!parsed.complete);
        assert_eq!(parsed.arrays["notes"].len(), 2);
        assert_eq!(parsed.arrays["notes"][0]["t"], "a, with } brace");

        assert_eq!(
            complete_records("prose {\"notes\":[{\"a\":1}", &["notes"])
                .arrays
                .len(),
            0
        );
    }

    #[test]
    fn keys_are_sorted_recursively() {
        let value = serde_json::json!({"b": 1, "a": {"d": [ {"z": 1, "y": 2} ], "c": 3}});
        assert_eq!(
            sort_keys(value).to_string(),
            r#"{"a":{"c":3,"d":[{"y":2,"z":1}]},"b":1}"#
        );
    }

    #[test]
    fn code_fences_are_stripped() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("plain"), "plain");
    }

    #[test]
    fn harmony_analysis_and_final_channels_are_stripped() {
        let raw = "<|start|>assistant<|channel|>analysis<|message|>thinking step<|end|><|start|>assistant<|channel|>final<|message|>{\"candidates\":[]}<|return|>";
        assert_eq!(strip_reasoning(raw), "{\"candidates\":[]}");
        let parsed = parse_object(raw).unwrap();
        assert_eq!(parsed["candidates"], serde_json::json!([]));
    }

    #[test]
    fn harmony_constrain_json_between_channel_and_message_is_handled() {
        let raw = "<|channel|>analysis<|message|>plan<|end|><|channel|>final<|constrain|>json<|message|>{\"ok\":true}<|return|>";
        assert_eq!(strip_reasoning(raw), "{\"ok\":true}");
    }

    #[test]
    fn harmony_tokens_combined_with_think_tags_are_stripped() {
        let raw = "<|channel|>analysis<|message|>reason<|end|><|start|>assistant<|channel|>final<|message|><think>more thinking</think>{\"x\":2}<|call|>";
        assert_eq!(strip_reasoning(raw), "{\"x\":2}");
    }

    #[test]
    fn truncated_harmony_analysis_hides_incomplete_reasoning() {
        let truncated = "Prefix <|channel|>analysis<|message|>incomplete thought";
        assert_eq!(visible_text_of_truncated(truncated), "Prefix");
        assert_eq!(
            strip_reasoning("<|channel|>analysis<|message|>incomplete"),
            ""
        );
        let final_truncated =
            "<|channel|>analysis<|message|>done<|end|><|channel|>final<|message|>{\"a\":1";
        assert_eq!(visible_text_of_truncated(final_truncated), "{\"a\":1");
    }

    #[test]
    fn harmony_preserves_control_tokens_inside_json_strings() {
        let raw = "<|start|>assistant<|channel|>analysis<|message|>plan<|end|><|start|>assistant<|channel|>final<|constrain|>json<|message|>{\"candidates\":[{\"segment_id\":1,\"wrong_in_primary\":\"foo<|end|>bar\",\"suggested_fix\":\"x<|return|>y\",\"supported_by\":\"context\",\"reason\":\"literal <|start|> token\"}]}<|return|>";
        let stripped = strip_reasoning(raw);
        assert!(stripped.contains("foo<|end|>bar"));
        assert!(stripped.contains("x<|return|>y"));
        assert!(stripped.contains("literal <|start|> token"));
        let parsed = parse_object(raw).unwrap();
        assert_eq!(parsed["candidates"][0]["wrong_in_primary"], "foo<|end|>bar");
        assert_eq!(parsed["candidates"][0]["suggested_fix"], "x<|return|>y");
    }

    #[test]
    fn nested_think_blocks_strip_outer_and_inner_while_keeping_unclosed() {
        let nested = "<think>outer {bad_json} <think>inner</think> still thinking {also_bad}</think>{\"ok\":true}";
        assert_eq!(strip_reasoning(nested), "{\"ok\":true}");
        assert_eq!(parse_object(nested).unwrap()["ok"], true);
        assert_eq!(strip_reasoning("<think>unclosed"), "<think>unclosed");
    }

    #[test]
    fn truncated_visible_text_strips_closed_blocks_before_truncation() {
        let final_with_closed_think = "<|channel|>analysis<|message|>plan<|end|><|channel|>final<|message|><think>thought</think>VisibleAnswer";
        assert_eq!(
            visible_text_of_truncated(final_with_closed_think),
            "VisibleAnswer"
        );
        let analysis_then_prose = "<|channel|>analysis<|message|>done<|end|>Partial prose";
        assert_eq!(
            visible_text_of_truncated(analysis_then_prose),
            "Partial prose"
        );
        let closed_then_unclosed = "<think>closed</think>Keep <think>unclosed";
        assert_eq!(visible_text_of_truncated(closed_then_unclosed), "Keep");
    }

    #[test]
    fn extract_json_object_skips_prose_braces() {
        let text = "Checking {segment_id=1} and {other}:\n{\"candidates\":[{\"segment_id\":1}]}";
        assert_eq!(
            extract_json_object(text),
            Some("{\"candidates\":[{\"segment_id\":1}]}")
        );
        let parsed = parse_object(text).unwrap();
        assert_eq!(parsed["candidates"][0]["segment_id"], 1);
    }

    #[test]
    fn harmony_constrain_does_not_eat_distant_prose() {
        let text = "<|constrain|>short note\nKeep this prose <|message|>after";
        let stripped = strip_reasoning(text);
        assert!(stripped.contains("Keep this prose"));
    }
}
