use std::collections::HashSet;

use anyhow::Result;
use serde_json::{Map, Value};

use lokalbot_ai::post_correction::{
    MultiSttSegment, PostCorrectionConfig, PostCorrectionOutput, PostCorrector,
};
use lokalbot_core::{Library, Meeting, Settings, Transcript};

pub const AUDIT_FILE_NAME: &str = "asr-post-correction-audit.json";

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn snake_to_camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper_next = false;
    for ch in key.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            out.push(ch.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn normalize_camel_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let camel = if k.contains('_') {
                    snake_to_camel(&k)
                } else {
                    k
                };
                out.insert(camel, normalize_camel_keys(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_camel_keys).collect()),
        other => other,
    }
}

pub fn think_api_key(settings: &Settings) -> Option<String> {
    let typed = settings.think_api_key.trim();
    if !typed.is_empty() {
        return Some(typed.to_string());
    }
    non_empty_str(settings.other.get("thinkApiKey"))
        .or_else(|| non_empty_str(settings.other.get("think_api_key")))
        .or_else(|| {
            settings.other.get("think").and_then(|think| {
                non_empty_str(think.get("apiKey")).or_else(|| non_empty_str(think.get("api_key")))
            })
        })
}

fn remove_nested_keys(settings: &mut Settings, parent: &str, keys: &[&str]) {
    let remove_parent = match settings.other.get_mut(parent) {
        Some(Value::Object(map)) => {
            for key in keys {
                map.remove(*key);
            }
            map.is_empty()
        }
        _ => false,
    };
    if remove_parent {
        settings.other.remove(parent);
    }
}

pub fn set_think_api_key(settings: &mut Settings, key: Option<&str>) {
    settings.other.remove("think_api_key");
    remove_nested_keys(settings, "think", &["apiKey", "api_key"]);
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => {
            settings.think_api_key = k.to_string();
            settings
                .other
                .insert("thinkApiKey".into(), Value::String(k.to_string()));
        }
        None => {
            settings.think_api_key.clear();
            settings.other.remove("thinkApiKey");
        }
    }
}

pub fn local_llama_server_disabled(settings: &Settings) -> bool {
    if settings.disable_local_llama_server {
        return true;
    }
    settings
        .other
        .get("disableLocalLlamaServer")
        .and_then(Value::as_bool)
        .or_else(|| {
            settings
                .other
                .get("disable_local_llama_server")
                .and_then(Value::as_bool)
        })
        .or_else(|| {
            settings.other.get("think").and_then(|think| {
                think
                    .get("disableLocalLlamaServer")
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        think
                            .get("disable_local_llama_server")
                            .and_then(Value::as_bool)
                    })
            })
        })
        .unwrap_or(false)
}

pub fn set_local_llama_server_disabled(settings: &mut Settings, disabled: bool) {
    settings.disable_local_llama_server = disabled;
    settings.other.remove("disable_local_llama_server");
    remove_nested_keys(
        settings,
        "think",
        &["disableLocalLlamaServer", "disable_local_llama_server"],
    );
    if disabled {
        settings
            .other
            .insert("disableLocalLlamaServer".into(), Value::Bool(true));
    } else {
        settings.other.remove("disableLocalLlamaServer");
    }
}

pub fn post_correction_config(settings: &Settings) -> Option<PostCorrectionConfig> {
    if let Some(raw) = settings
        .other
        .get("postCorrection")
        .or_else(|| settings.other.get("post_correction"))
        .or_else(|| {
            settings.other.get("transcription").and_then(|tx| {
                tx.get("postCorrection")
                    .or_else(|| tx.get("post_correction"))
            })
        })
    {
        let normalized = normalize_camel_keys(raw.clone());
        if let Ok(mut cfg) = serde_json::from_value::<PostCorrectionConfig>(normalized) {
            if cfg.api_key.trim().is_empty() && !settings.post_correction_api_key.trim().is_empty()
            {
                cfg.api_key = settings.post_correction_api_key.trim().to_string();
            }
            return Some(cfg);
        }
    }
    if settings.post_correction_enabled
        || !settings.post_correction_base_url.trim().is_empty()
        || !settings.post_correction_api_key.trim().is_empty()
    {
        let defaults = PostCorrectionConfig::default();
        return Some(PostCorrectionConfig {
            enabled: settings.post_correction_enabled,
            base_url: settings.post_correction_base_url.trim().to_string(),
            api_key: settings.post_correction_api_key.trim().to_string(),
            primary_judge_model: if settings
                .post_correction_primary_judge_model
                .trim()
                .is_empty()
            {
                defaults.primary_judge_model
            } else {
                settings
                    .post_correction_primary_judge_model
                    .trim()
                    .to_string()
            },
            support_critic_models: if settings.post_correction_support_critic_models.is_empty() {
                defaults.support_critic_models
            } else {
                settings.post_correction_support_critic_models.clone()
            },
            segments_per_batch: settings.post_correction_segments_per_batch.max(1),
            request_timeout_sec: settings.post_correction_request_timeout_sec.max(1),
            max_concurrent_batches: settings.post_correction_max_concurrent_batches.max(1),
            continue_on_critic_failure: settings.post_correction_continue_on_critic_failure,
        });
    }
    None
}

pub fn set_post_correction_config(settings: &mut Settings, config: Option<&PostCorrectionConfig>) {
    settings.other.remove("post_correction");
    remove_nested_keys(
        settings,
        "transcription",
        &["postCorrection", "post_correction"],
    );
    match config {
        Some(cfg) => {
            settings.post_correction_enabled = cfg.enabled;
            settings.post_correction_base_url = cfg.base_url.clone();
            settings.post_correction_api_key = cfg.api_key.clone();
            settings.post_correction_primary_judge_model = cfg.primary_judge_model.clone();
            settings.post_correction_support_critic_models = cfg.support_critic_models.clone();
            settings.post_correction_segments_per_batch = cfg.segments_per_batch.max(1);
            settings.post_correction_request_timeout_sec = cfg.request_timeout_sec.max(1);
            settings.post_correction_max_concurrent_batches = cfg.max_concurrent_batches.max(1);
            settings.post_correction_continue_on_critic_failure = cfg.continue_on_critic_failure;
            if let Ok(val) = serde_json::to_value(cfg) {
                settings.other.insert("postCorrection".into(), val);
            }
        }
        None => {
            settings.post_correction_enabled = false;
            settings.post_correction_base_url.clear();
            settings.post_correction_api_key.clear();
            settings.other.remove("postCorrection");
        }
    }
}

pub fn save_audit_log(settings: &Settings) -> bool {
    settings
        .other
        .get("saveAuditLog")
        .and_then(Value::as_bool)
        .or_else(|| {
            settings
                .other
                .get("save_audit_log")
                .and_then(Value::as_bool)
        })
        .or_else(|| {
            settings.other.get("transcription").and_then(|tx| {
                tx.get("saveAuditLog")
                    .and_then(Value::as_bool)
                    .or_else(|| tx.get("save_audit_log").and_then(Value::as_bool))
            })
        })
        .or_else(|| {
            settings
                .other
                .get("postCorrection")
                .or_else(|| settings.other.get("post_correction"))
                .and_then(|pc| {
                    pc.get("saveAuditLog")
                        .and_then(Value::as_bool)
                        .or_else(|| pc.get("save_audit_log").and_then(Value::as_bool))
                })
        })
        .unwrap_or(settings.transcription_save_audit_log)
}

pub fn set_save_audit_log(settings: &mut Settings, save: bool) {
    settings.transcription_save_audit_log = save;
    settings.other.remove("save_audit_log");
    remove_nested_keys(
        settings,
        "transcription",
        &["saveAuditLog", "save_audit_log"],
    );
    remove_nested_keys(
        settings,
        "postCorrection",
        &["saveAuditLog", "save_audit_log"],
    );
    remove_nested_keys(
        settings,
        "post_correction",
        &["saveAuditLog", "save_audit_log"],
    );
    if save {
        settings
            .other
            .insert("saveAuditLog".into(), Value::Bool(true));
    } else {
        settings.other.remove("saveAuditLog");
    }
}

pub fn segments_from_transcript(transcript: &Transcript) -> Vec<MultiSttSegment> {
    transcript
        .segments
        .iter()
        .enumerate()
        .map(|(index, segment)| MultiSttSegment {
            segment_id: index + 1,
            start: segment.start,
            end: segment.end,
            speaker: segment.speaker.clone(),
            primary_text: segment.text.clone(),
            tr_backup_text: String::new(),
            fast_helper_text: String::new(),
        })
        .collect()
}

pub fn segments_from_multi_stt(
    segments: &[lokalbot_speech::MultiSttSegment],
) -> Vec<MultiSttSegment> {
    segments
        .iter()
        .enumerate()
        .map(|(index, segment)| MultiSttSegment {
            segment_id: index + 1,
            start: segment.start,
            end: segment.end,
            speaker: segment.speaker.clone(),
            primary_text: segment.primary.clone(),
            tr_backup_text: segment.tr_backup.clone(),
            fast_helper_text: segment.fast_helper.clone(),
        })
        .collect()
}

pub fn vocabulary_terms(meeting: &Meeting, library: &Library, settings: &Settings) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    let mut push_term = |term: &str| {
        let trimmed = term.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            terms.push(trimmed.to_string());
        }
    };

    let folder = library.meeting_dir(meeting);
    if let Some(record) = lokalbot_core::vocabulary::load(&folder) {
        for term in record.terms {
            push_term(&term);
        }
    } else if settings.auto_transcription_vocabulary && !meeting.is_sample() {
        let meetings = library.list_all_meetings().unwrap_or_default();
        for term in lokalbot_core::vocabulary::terms(&lokalbot_core::vocabulary::sources(
            meeting, &meetings, library,
        )) {
            push_term(&term);
        }
    }

    for part in settings.transcription_prompt.split([',', '\n', ';']) {
        push_term(part);
    }
    terms
}

pub fn correct_transcript(
    meeting: &Meeting,
    library: &Library,
    settings: &Settings,
    transcript: Transcript,
    segments: &[MultiSttSegment],
    on_start: &mut dyn FnMut(),
) -> Result<(Transcript, bool)> {
    correct_transcript_with_keyring(
        meeting,
        library,
        settings,
        transcript,
        segments,
        crate::secrets::openai_api_key(),
        on_start,
    )
}

pub(crate) fn correct_transcript_with_keyring(
    meeting: &Meeting,
    library: &Library,
    settings: &Settings,
    mut transcript: Transcript,
    segments: &[MultiSttSegment],
    keyring_fallback: Option<String>,
    on_start: &mut dyn FnMut(),
) -> Result<(Transcript, bool)> {
    let Some(config) = post_correction_config(settings) else {
        return Ok((transcript, false));
    };
    if !config.enabled || transcript.segments.is_empty() || segments.is_empty() {
        return Ok((transcript, false));
    }
    on_start();
    if !config.api_key.trim().is_empty() {
        lokalbot_core::text::register_redacted_secrets([config.api_key.as_str()]);
    }

    let vocab = vocabulary_terms(meeting, library, settings);
    let corrector = PostCorrector::new(config, settings.approved_remote_inference_origins.clone())?
        .with_vocabulary(vocab)
        .with_openrouter_data_policy(settings.open_router_data_policy)
        .with_keyring_fallback(keyring_fallback);
    let output: PostCorrectionOutput = corrector.correct(segments)?;
    output.apply_to_transcript(&mut transcript)?;

    if save_audit_log(settings) {
        let audit_path = library.meeting_dir(meeting).join(AUDIT_FILE_NAME);
        lokalbot_core::library::write_json(&audit_path, &output.audit)?;
    }

    Ok((transcript, true))
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use parking_lot::Mutex;
    use serde_json::{Value, json};

    use lokalbot_core::model::transcript::{Segment, TimingPrecision};
    use lokalbot_core::settings::SummarizerBackend;
    use lokalbot_core::{Library, LibraryPaths};

    use super::*;
    use crate::Shared;
    use crate::models::Models;

    #[derive(Clone, Debug)]
    struct MockRequest {
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockRequest {
        fn header(&self, name: &str) -> Option<String> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }
    }

    struct MockServer {
        port: u16,
        requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    impl MockServer {
        fn start(handler: impl Fn(&MockRequest) -> (u16, Value) + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            let handler = Arc::new(handler);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let log = log.clone();
                    let handler = handler.clone();
                    std::thread::spawn(move || {
                        let _ = handle_conn(stream, &log, &*handler);
                    });
                }
            });
            Self { port, requests }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().clone()
        }
    }

    fn handle_conn(
        mut stream: TcpStream,
        log: &Mutex<Vec<MockRequest>>,
        handler: &(dyn Fn(&MockRequest) -> (u16, Value) + Send + Sync),
    ) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut first = String::new();
        reader.read_line(&mut first)?;
        let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let len = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        let req = MockRequest {
            path,
            headers,
            body,
        };
        log.lock().push(req.clone());
        let (status, payload) = handler(&req);
        let bytes = payload.to_string().into_bytes();
        let head = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&bytes)?;
        stream.flush()
    }

    fn chat_json(content: &str) -> (u16, Value) {
        (
            200,
            json!({
                "choices": [{
                    "message": { "role": "assistant", "content": content },
                    "finish_reason": "stop"
                }]
            }),
        )
    }

    #[test]
    fn settings_other_parsing_redaction_and_disable_local_llama_server() {
        let mut settings = Settings::default();
        settings.other.insert(
            "think".into(),
            json!({
                "api_key": "sk-nested-think-key",
                "disable_local_llama_server": true
            }),
        );
        settings.other.insert(
            "transcription".into(),
            json!({
                "save_audit_log": true,
                "post_correction": {
                    "enabled": true,
                    "base_url": "http://127.0.0.1:9999/v1",
                    "api_key": "sk-nested-pc-key",
                    "primary_judge_model": "Qwen3.8-27B",
                    "support_critic_models": ["gemma-4-31B-it", "gpt-oss-120b"],
                    "segments_per_batch": 6,
                    "request_timeout_sec": 45,
                    "max_concurrent_batches": 2,
                    "continue_on_critic_failure": true
                }
            }),
        );

        assert_eq!(
            think_api_key(&settings).as_deref(),
            Some("sk-nested-think-key")
        );
        assert!(local_llama_server_disabled(&settings));
        assert!(save_audit_log(&settings));
        let pc = post_correction_config(&settings).unwrap();
        assert!(pc.enabled);
        assert_eq!(pc.api_key, "sk-nested-pc-key");
        assert_eq!(pc.segments_per_batch, 6);

        let sanitized = crate::diagnostics::sanitized_settings(&settings).unwrap();
        let serialized = String::from_utf8(sanitized).unwrap();
        assert!(!serialized.contains("sk-nested-think-key"));
        assert!(!serialized.contains("sk-nested-pc-key"));

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let shared = Shared::for_tests(library, settings, |_| {});
        assert!(shared.runtime.is_local_server_disabled());
        let models = Models::new(shared.clone());
        assert!(models.think_requirements().is_empty());
        models.finish_runtime_update();
        assert!(!models.runtime_updating());
        assert!(shared.think(&shared.settings()).is_err());
    }

    #[test]
    fn api_key_precedence_and_remote_think_work_without_local_server() {
        let server = MockServer::start(|_| chat_json("remote summary response"));
        let mut settings = Settings {
            summarizer_backend: SummarizerBackend::OpenAICompatible,
            openai_base_url: server.url("/v1"),
            openai_model: "Qwen3.8-27B".into(),
            ..Default::default()
        };
        set_think_api_key(&mut settings, Some("sk-config-think-secret"));
        set_local_llama_server_disabled(&mut settings, true);

        assert_eq!(
            crate::secrets::resolve_openai_api_key_with(&settings, || Some("sk-keyring".into())),
            Some("sk-config-think-secret".into())
        );

        let mut empty_cfg = settings.clone();
        set_think_api_key(&mut empty_cfg, None);
        assert_eq!(
            crate::secrets::resolve_openai_api_key_with(&empty_cfg, || Some("sk-keyring".into())),
            Some("sk-keyring".into())
        );

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let shared = Shared::for_tests(library, settings.clone(), |_| {});
        let client = shared.think(&settings).unwrap();
        let reply = client
            .complete(&lokalbot_ai::ChatRequest::new("sys", "usr"))
            .unwrap();
        assert_eq!(reply, "remote summary response");
        let req = server.requests().pop().unwrap();
        assert_eq!(req.path, "/v1/chat/completions");
        assert_eq!(
            req.header("authorization").as_deref(),
            Some("Bearer sk-config-think-secret")
        );
    }

    #[test]
    fn audit_json_is_written_only_when_save_audit_log_is_enabled() {
        let server = MockServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "gemma-4-31B-it" => chat_json(
                    r#"{"candidates":[{"segment_id":1,"wrong_in_primary":"LocalBOT","suggested_fix":"LokalBot","supported_by":"whisper-turbo-tr-khan","reason":"name"}]}"#,
                ),
                "gpt-oss-120b" => chat_json(r#"{"candidates":[]}"#),
                "Qwen3.8-27B" => chat_json(
                    r#"{"segments":[{"segment_id":1,"text":"LokalBot kurulumu"},{"segment_id":2,"text":"ikinci satır"}]}"#,
                ),
                _ => (404, json!({"error": "unknown"})),
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let meeting = library
            .create_meeting("LokalBot Sync", "Teams", Utc::now())
            .unwrap();
        let transcript = Transcript {
            segments: vec![
                Segment {
                    start: 0.0,
                    end: 2.5,
                    speaker: "Speaker 1".into(),
                    text: "LocalBOT kurulumu".into(),
                    confidence: Some(0.95),
                    timing_precision: Some(TimingPrecision::Token),
                    attribution: None,
                },
                Segment {
                    start: 2.5,
                    end: 5.0,
                    speaker: "Speaker 2".into(),
                    text: "ikinci satır".into(),
                    confidence: Some(0.91),
                    timing_precision: Some(TimingPrecision::Token),
                    attribution: None,
                },
            ],
            ..Default::default()
        };

        let mut settings = Settings::default();
        let pc_cfg = PostCorrectionConfig {
            enabled: true,
            base_url: server.url("/v1"),
            api_key: "sk-audit-secret-key".into(),
            ..Default::default()
        };
        set_post_correction_config(&mut settings, Some(&pc_cfg));
        set_save_audit_log(&mut settings, true);

        let segments = segments_from_transcript(&transcript);
        let mut stage_count = 0usize;
        let (corrected, ran) = correct_transcript_with_keyring(
            &meeting,
            &library,
            &settings,
            transcript.clone(),
            &segments,
            None,
            &mut || stage_count += 1,
        )
        .unwrap();
        assert!(ran);
        assert_eq!(stage_count, 1);
        assert_eq!(corrected.segments.len(), 2);
        assert_eq!(corrected.segments[0].text, "LokalBot kurulumu");
        assert_eq!(corrected.segments[0].start, 0.0);
        assert_eq!(corrected.segments[0].end, 2.5);
        assert_eq!(corrected.segments[0].speaker, "Speaker 1");
        assert_eq!(corrected.segments[0].confidence, Some(0.95));

        let audit_path = library.meeting_dir(&meeting).join(AUDIT_FILE_NAME);
        assert!(audit_path.is_file());
        let audit_raw = std::fs::read_to_string(&audit_path).unwrap();
        assert!(!audit_raw.contains("sk-audit-secret-key"));
        assert!(audit_raw.contains("LokalBot kurulumu"));

        let dir2 = tempfile::tempdir().unwrap();
        let lib2 = Library::open(LibraryPaths::new(dir2.path())).unwrap();
        let meeting2 = lib2
            .create_meeting("No Audit", "Teams", Utc::now())
            .unwrap();
        set_save_audit_log(&mut settings, false);
        let (_, ran2) = correct_transcript_with_keyring(
            &meeting2,
            &lib2,
            &settings,
            transcript,
            &segments,
            None,
            &mut || {},
        )
        .unwrap();
        assert!(ran2);
        assert!(!lib2.meeting_dir(&meeting2).join(AUDIT_FILE_NAME).exists());
    }

    #[test]
    fn save_audit_log_supports_snake_case_post_correction_and_clears_nested_keys() {
        let mut settings = Settings::default();
        settings.other.insert(
            "post_correction".into(),
            json!({ "save_audit_log": true, "enabled": true }),
        );
        assert!(save_audit_log(&settings));

        settings.other.insert(
            "transcription".into(),
            json!({ "save_audit_log": true, "language": "tr" }),
        );
        settings
            .other
            .insert("postCorrection".into(), json!({ "saveAuditLog": true }));
        set_save_audit_log(&mut settings, false);
        assert!(!save_audit_log(&settings));
        assert_eq!(
            settings
                .other
                .get("transcription")
                .and_then(|v| v.get("language"))
                .and_then(Value::as_str),
            Some("tr")
        );
        assert!(!settings.other.contains_key("postCorrection"));

        settings
            .other
            .insert("post_correction".into(), json!({ "save_audit_log": false }));
        assert!(!save_audit_log(&settings));
    }

    #[test]
    fn setters_clear_nested_think_and_transcription_keys() {
        let mut settings = Settings::default();
        settings.other.insert(
            "think".into(),
            json!({
                "api_key": "sk-old-nested-think",
                "disable_local_llama_server": true,
                "model": "Qwen3.8-27B"
            }),
        );
        settings.other.insert(
            "transcription".into(),
            json!({
                "mode": "asymmetric_multi_stt",
                "post_correction": {
                    "enabled": true,
                    "base_url": "http://127.0.0.1:9999/v1"
                }
            }),
        );

        assert_eq!(
            think_api_key(&settings).as_deref(),
            Some("sk-old-nested-think")
        );
        assert!(local_llama_server_disabled(&settings));
        assert!(post_correction_config(&settings).is_some());

        set_think_api_key(&mut settings, None);
        assert_eq!(think_api_key(&settings), None);

        set_local_llama_server_disabled(&mut settings, false);
        assert!(!local_llama_server_disabled(&settings));

        set_post_correction_config(&mut settings, None);
        assert!(post_correction_config(&settings).is_none());

        assert_eq!(
            settings
                .other
                .get("think")
                .and_then(|v| v.get("model"))
                .and_then(Value::as_str),
            Some("Qwen3.8-27B")
        );
        assert_eq!(
            settings
                .other
                .get("transcription")
                .and_then(|v| v.get("mode"))
                .and_then(Value::as_str),
            Some("asymmetric_multi_stt")
        );

        set_think_api_key(&mut settings, Some("sk-new-top"));
        set_think_api_key(&mut settings, Some("   "));
        assert_eq!(think_api_key(&settings), None);
    }

    #[test]
    fn typed_settings_and_config_profile_drive_post_correction_and_think() {
        let server = MockServer::start(|req| {
            let body = req.json();
            match body["model"].as_str().unwrap_or("") {
                "gemma-4-31B-it" => chat_json(
                    r#"{"candidates":[{"segment_id":1,"wrong_in_primary":"lokal bot","suggested_fix":"LokalBot","supported_by":"zipformer-oayk","reason":"brand"}]}"#,
                ),
                "gpt-oss-120b" => chat_json(r#"{"candidates":[]}"#),
                "Qwen3.8-27B" => {
                    chat_json(r#"{"segments":[{"segment_id":1,"text":"LokalBot hazır"}]}"#)
                }
                _ => (404, json!({"error": "unknown"})),
            }
        });

        let toml_text = format!(
            r#"
schema_version = 1

[deployment]
auto_approve_configured_origins = true
skip_onboarding_wizard = true

[transcription]
mode = "asymmetric_multi_stt"
language = "tr"
live_preview_during_meeting = false
save_audit_log = true
cpu_threads = 6
auto_fallback_to_cpu = true

[transcription.post_correction]
enabled = true
base_url = "{url}"
api_key = "sk-typed-pc-key"
primary_judge_model = "Qwen3.8-27B"
support_critic_models = ["gemma-4-31B-it", "gpt-oss-120b"]
segments_per_batch = 5
request_timeout_sec = 30
max_concurrent_batches = 2
continue_on_critic_failure = true

[think]
backend = "openai_compatible"
base_url = "{url}"
api_key = "sk-typed-think-key"
model = "Qwen3.8-27B"
disable_local_llama_server = true
"#,
            url = server.url("/v1")
        );

        let profile =
            lokalbot_core::config_profile::ConfigProfile::import_toml(&toml_text, None).unwrap();
        let mut settings = Settings::default();
        profile.apply_to_settings(&mut settings);

        assert_eq!(
            think_api_key(&settings).as_deref(),
            Some("sk-typed-think-key")
        );
        assert!(local_llama_server_disabled(&settings));
        assert!(save_audit_log(&settings));
        let pc_cfg = post_correction_config(&settings).unwrap();
        assert!(pc_cfg.enabled);
        assert_eq!(pc_cfg.api_key, "sk-typed-pc-key");

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let meeting = library
            .create_meeting("Typed Sync", "Teams", Utc::now())
            .unwrap();
        let transcript = Transcript {
            segments: vec![Segment {
                start: 0.0,
                end: 2.0,
                speaker: "Speaker 1".into(),
                text: "lokal bot hazır".into(),
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
            primary: "lokal bot hazır".into(),
            tr_backup: "Lokal bot hazır".into(),
            fast_helper: "LokalBot hazır".into(),
            timing_precision: None,
            attribution: None,
        }];
        let mapped = segments_from_multi_stt(&multi);
        assert_eq!(mapped[0].segment_id, 1);
        assert_eq!(mapped[0].tr_backup_text, "Lokal bot hazır");
        assert_eq!(mapped[0].fast_helper_text, "LokalBot hazır");

        let (corrected, ran) = correct_transcript_with_keyring(
            &meeting,
            &library,
            &settings,
            transcript,
            &mapped,
            None,
            &mut || {},
        )
        .unwrap();
        assert!(ran);
        assert_eq!(corrected.segments[0].text, "LokalBot hazır");
        assert!(
            library
                .meeting_dir(&meeting)
                .join(AUDIT_FILE_NAME)
                .is_file()
        );
    }
}
