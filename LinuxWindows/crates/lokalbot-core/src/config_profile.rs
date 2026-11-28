use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::access::origin_of;
use crate::library::write_atomic;
use crate::paths::{CONFIG_FILE_ENV, CONFIG_FILE_NAME, LibraryPaths};
use crate::settings::{Settings, SummarizerBackend};
use crate::text::{API_KEY_MASK, is_masked_api_key, mask_api_key, register_redacted_secrets};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionMode {
    #[default]
    Single,
    AsymmetricMultiStt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttModelSlot {
    pub id: String,
    pub device: String,
}

impl Default for SttModelSlot {
    fn default() -> Self {
        Self {
            id: String::new(),
            device: "cpu".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeploymentConfig {
    pub auto_approve_configured_origins: bool,
    pub skip_onboarding_wizard: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptionModelsConfig {
    pub primary: SttModelSlot,
    pub tr_backup: SttModelSlot,
    pub fast_helper: SttModelSlot,
}

impl Default for TranscriptionModelsConfig {
    fn default() -> Self {
        Self {
            primary: SttModelSlot {
                id: "whisper-turbo".into(),
                device: "igpu".into(),
            },
            tr_backup: SttModelSlot {
                id: "whisper-turbo-tr-khan".into(),
                device: "igpu".into(),
            },
            fast_helper: SttModelSlot {
                id: "zipformer-oayk".into(),
                device: "cpu".into(),
            },
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
        f.debug_struct("PostCorrectionConfig")
            .field("enabled", &self.enabled)
            .field("base_url", &self.base_url)
            .field("api_key", &mask_api_key(&self.api_key))
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkConfig {
    pub backend: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub disable_local_llama_server: bool,
}

impl Default for ThinkConfig {
    fn default() -> Self {
        Self {
            backend: "built_in".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: "Qwen3.8-27B".into(),
            disable_local_llama_server: false,
        }
    }
}

impl fmt::Debug for ThinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThinkConfig")
            .field("backend", &self.backend)
            .field("base_url", &self.base_url)
            .field("api_key", &mask_api_key(&self.api_key))
            .field("model", &self.model)
            .field(
                "disable_local_llama_server",
                &self.disable_local_llama_server,
            )
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptionConfig {
    pub mode: TranscriptionMode,
    pub language: String,
    pub live_preview_during_meeting: bool,
    pub save_audit_log: bool,
    pub cpu_threads: usize,
    pub auto_fallback_to_cpu: bool,
    pub models: TranscriptionModelsConfig,
    pub post_correction: PostCorrectionConfig,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            mode: TranscriptionMode::Single,
            language: "tr".into(),
            live_preview_during_meeting: false,
            save_audit_log: true,
            cpu_threads: 6,
            auto_fallback_to_cpu: true,
            models: TranscriptionModelsConfig::default(),
            post_correction: PostCorrectionConfig::default(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigProfile {
    pub schema_version: u32,
    pub deployment: DeploymentConfig,
    pub transcription: TranscriptionConfig,
    pub think: ThinkConfig,
}

impl Default for ConfigProfile {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            deployment: DeploymentConfig::default(),
            transcription: TranscriptionConfig::default(),
            think: ThinkConfig::default(),
        }
    }
}

impl fmt::Debug for ConfigProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigProfile")
            .field("schema_version", &self.schema_version)
            .field("deployment", &self.deployment)
            .field("transcription", &self.transcription)
            .field("think", &self.think)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigResolution {
    pub active_path: PathBuf,
    pub seeded_from: Option<PathBuf>,
    pub is_explicit: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedConfig {
    pub profile: ConfigProfile,
    pub resolution: ConfigResolution,
}

fn validate_url_if_present(label: &str, url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let Some(origin) = origin_of(trimmed) else {
        bail!("{label} is not a valid URL");
    };
    if !origin.starts_with("http://") && !origin.starts_with("https://") {
        bail!("{label} must use http:// or https://");
    }
    Ok(())
}

fn normalized_think_backend(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "built_in" | "builtin" => Some("built_in"),
        "openai_compatible" | "openai-compatible" | "openai" => Some("openai_compatible"),
        "ollama" => Some("ollama"),
        _ => None,
    }
}

fn backend_name_from_settings(backend: SummarizerBackend) -> &'static str {
    match backend {
        SummarizerBackend::BuiltIn => "built_in",
        SummarizerBackend::Ollama => "ollama",
        SummarizerBackend::OpenAICompatible
        | SummarizerBackend::ChatGptPlan
        | SummarizerBackend::ClaudePlan
        | SummarizerBackend::AntigravityPlan
        | SummarizerBackend::GeminiPlan => "openai_compatible",
    }
}

fn add_origin(approved: &mut Vec<String>, url: &str) {
    let Some(origin) = origin_of(url).filter(|o| o.starts_with("https://")) else {
        return;
    };
    if !approved
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&origin))
    {
        approved.push(origin);
    }
}

fn remove_other_nested_keys(settings: &mut Settings, parent: &str, keys: &[&str]) {
    let remove_parent = match settings.other.get_mut(parent) {
        Some(serde_json::Value::Object(map)) => {
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

fn clear_legacy_other_overrides(settings: &mut Settings) {
    for key in [
        "postCorrection",
        "post_correction",
        "saveAuditLog",
        "save_audit_log",
        "disableLocalLlamaServer",
        "disable_local_llama_server",
        "thinkApiKey",
        "think_api_key",
        "multiStt",
        "multi_stt",
    ] {
        settings.other.remove(key);
    }
    remove_other_nested_keys(
        settings,
        "transcription",
        &[
            "mode",
            "postCorrection",
            "post_correction",
            "saveAuditLog",
            "save_audit_log",
        ],
    );
    remove_other_nested_keys(
        settings,
        "think",
        &[
            "apiKey",
            "api_key",
            "disableLocalLlamaServer",
            "disable_local_llama_server",
        ],
    );
}

impl ConfigProfile {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            bail!(
                "unsupported lokalbot-config.toml schema_version {} (expected {})",
                self.schema_version,
                CURRENT_SCHEMA_VERSION
            );
        }
        if self.transcription.cpu_threads == 0 {
            bail!("transcription.cpu_threads must be at least 1");
        }
        if self.transcription.models.primary.id.trim().is_empty()
            || self.transcription.models.tr_backup.id.trim().is_empty()
            || self.transcription.models.fast_helper.id.trim().is_empty()
        {
            bail!("transcription.models ids must not be empty");
        }
        let pc = &self.transcription.post_correction;
        if pc.segments_per_batch == 0 {
            bail!("transcription.post_correction.segments_per_batch must be at least 1");
        }
        if pc.request_timeout_sec == 0 {
            bail!("transcription.post_correction.request_timeout_sec must be at least 1");
        }
        if pc.max_concurrent_batches == 0 {
            bail!("transcription.post_correction.max_concurrent_batches must be at least 1");
        }
        validate_url_if_present("transcription.post_correction.base_url", &pc.base_url)?;
        if normalized_think_backend(&self.think.backend).is_none() {
            bail!("unsupported think.backend '{}'", self.think.backend);
        }
        validate_url_if_present("think.base_url", &self.think.base_url)?;
        Ok(())
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read config file {}", path.display()))?;
        let mut profile: Self = toml::from_str(&raw)
            .with_context(|| format!("could not parse TOML in {}", path.display()))?;
        if is_masked_api_key(&profile.transcription.post_correction.api_key) {
            profile.transcription.post_correction.api_key.clear();
        }
        if is_masked_api_key(&profile.think.api_key) {
            profile.think.api_key.clear();
        }
        profile.validate()?;
        Ok(profile)
    }

    pub fn resolve_and_load(
        explicit: Option<&Path>,
        env_override: Option<&Path>,
        paths: &LibraryPaths,
        exe_dir: Option<&Path>,
    ) -> Result<Option<LoadedConfig>> {
        if let Some(path) = explicit {
            let profile = Self::load_from_path(path)?;
            return Ok(Some(LoadedConfig {
                profile,
                resolution: ConfigResolution {
                    active_path: path.to_path_buf(),
                    seeded_from: None,
                    is_explicit: true,
                },
            }));
        }
        if let Some(path) = env_override.filter(|p| !p.as_os_str().is_empty()) {
            let profile = Self::load_from_path(path)?;
            return Ok(Some(LoadedConfig {
                profile,
                resolution: ConfigResolution {
                    active_path: path.to_path_buf(),
                    seeded_from: None,
                    is_explicit: true,
                },
            }));
        }
        let user_path = paths.config_file();
        if user_path.is_file() {
            let profile = Self::load_from_path(&user_path)?;
            return Ok(Some(LoadedConfig {
                profile,
                resolution: ConfigResolution {
                    active_path: user_path,
                    seeded_from: None,
                    is_explicit: false,
                },
            }));
        }
        if let Some(template_path) = exe_dir.map(|dir| dir.join(CONFIG_FILE_NAME))
            && template_path.is_file()
        {
            let raw = std::fs::read_to_string(&template_path).with_context(|| {
                format!("could not read config template {}", template_path.display())
            })?;
            let mut profile: Self = toml::from_str(&raw).with_context(|| {
                format!(
                    "could not parse config template {}",
                    template_path.display()
                )
            })?;
            let mut stripped_mask = false;
            if is_masked_api_key(&profile.transcription.post_correction.api_key) {
                profile.transcription.post_correction.api_key.clear();
                stripped_mask = true;
            }
            if is_masked_api_key(&profile.think.api_key) {
                profile.think.api_key.clear();
                stripped_mask = true;
            }
            profile.validate()?;
            let to_write = if stripped_mask {
                toml::to_string_pretty(&profile)
                    .context("failed to serialize sanitized config template")?
            } else {
                raw
            };
            write_atomic(&user_path, to_write.as_bytes()).with_context(|| {
                format!("could not seed config file to {}", user_path.display())
            })?;
            return Ok(Some(LoadedConfig {
                profile,
                resolution: ConfigResolution {
                    active_path: user_path,
                    seeded_from: Some(template_path),
                    is_explicit: false,
                },
            }));
        }
        Ok(None)
    }

    pub fn resolve_for_app(
        explicit: Option<&Path>,
        paths: &LibraryPaths,
    ) -> Result<Option<LoadedConfig>> {
        let env_path = std::env::var_os(CONFIG_FILE_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        Self::resolve_and_load(explicit, env_path.as_deref(), paths, exe_dir.as_deref())
    }

    pub fn apply_to_settings(&self, settings: &mut Settings) {
        clear_legacy_other_overrides(settings);
        register_redacted_secrets([
            self.transcription.post_correction.api_key.as_str(),
            self.think.api_key.as_str(),
        ]);
        if self.deployment.skip_onboarding_wizard {
            settings.onboarding_completed = true;
        }
        settings.transcription_mode = self.transcription.mode;
        if !self.transcription.language.trim().is_empty() {
            settings.transcription_language = self.transcription.language.trim().to_string();
        }
        settings.live_preview_during_meeting = self.transcription.live_preview_during_meeting;
        settings.transcription_save_audit_log = self.transcription.save_audit_log;
        settings.transcription_cpu_threads = self.transcription.cpu_threads.max(1);
        settings.transcription_auto_fallback_to_cpu = self.transcription.auto_fallback_to_cpu;
        settings.transcription_models = self.transcription.models.clone();

        let pc = &self.transcription.post_correction;
        settings.post_correction_enabled = pc.enabled;
        settings.post_correction_base_url = pc.base_url.trim().to_string();
        settings.post_correction_primary_judge_model = pc.primary_judge_model.trim().to_string();
        settings.post_correction_support_critic_models = pc
            .support_critic_models
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        settings.post_correction_segments_per_batch = pc.segments_per_batch.max(1);
        settings.post_correction_request_timeout_sec = pc.request_timeout_sec.max(1);
        settings.post_correction_max_concurrent_batches = pc.max_concurrent_batches.max(1);
        settings.post_correction_continue_on_critic_failure = pc.continue_on_critic_failure;
        settings.post_correction_api_key = pc.api_key.clone();

        let think_base = self.think.base_url.trim();
        let think_model = self.think.model.trim();
        match normalized_think_backend(&self.think.backend) {
            Some("openai_compatible") => {
                settings.summarizer_backend = SummarizerBackend::OpenAICompatible;
                if !think_base.is_empty() {
                    settings.openai_base_url = think_base.to_string();
                }
                settings.openai_model = think_model.to_string();
            }
            Some("ollama") => {
                settings.summarizer_backend = SummarizerBackend::Ollama;
                if !think_base.is_empty() {
                    settings.ollama_base_url = think_base.to_string();
                }
                settings.ollama_model = think_model.to_string();
            }
            _ => {
                settings.summarizer_backend = SummarizerBackend::BuiltIn;
            }
        }
        settings.disable_local_llama_server = self.think.disable_local_llama_server;
        settings.think_api_key = self.think.api_key.clone();

        if self.deployment.auto_approve_configured_origins {
            add_origin(
                &mut settings.approved_remote_inference_origins,
                &self.think.base_url,
            );
            add_origin(
                &mut settings.approved_remote_inference_origins,
                &self.transcription.post_correction.base_url,
            );
        }
    }

    pub fn from_settings(settings: &Settings, base: Option<&ConfigProfile>) -> Self {
        let mut profile = base.cloned().unwrap_or_default();
        profile.sync_from_settings(settings);
        profile
    }

    pub fn sync_from_settings(&mut self, settings: &Settings) {
        self.schema_version = CURRENT_SCHEMA_VERSION;
        self.transcription.mode = settings.transcription_mode;
        if !settings.transcription_language.trim().is_empty() {
            self.transcription.language = settings.transcription_language.clone();
        }
        self.transcription.live_preview_during_meeting = settings.live_preview_during_meeting;
        self.transcription.save_audit_log = settings.transcription_save_audit_log;
        self.transcription.cpu_threads = settings.transcription_cpu_threads.max(1);
        self.transcription.auto_fallback_to_cpu = settings.transcription_auto_fallback_to_cpu;
        self.transcription.models = settings.transcription_models.clone();

        self.transcription.post_correction.enabled = settings.post_correction_enabled;
        self.transcription.post_correction.base_url = settings.post_correction_base_url.clone();
        if !settings.post_correction_api_key.is_empty() {
            self.transcription.post_correction.api_key = settings.post_correction_api_key.clone();
        }
        self.transcription.post_correction.primary_judge_model =
            settings.post_correction_primary_judge_model.clone();
        self.transcription.post_correction.support_critic_models =
            settings.post_correction_support_critic_models.clone();
        self.transcription.post_correction.segments_per_batch =
            settings.post_correction_segments_per_batch.max(1);
        self.transcription.post_correction.request_timeout_sec =
            settings.post_correction_request_timeout_sec.max(1);
        self.transcription.post_correction.max_concurrent_batches =
            settings.post_correction_max_concurrent_batches.max(1);
        self.transcription
            .post_correction
            .continue_on_critic_failure = settings.post_correction_continue_on_critic_failure;

        self.think.backend = backend_name_from_settings(settings.summarizer_backend).into();
        match settings.summarizer_backend {
            SummarizerBackend::Ollama => {
                self.think.base_url = settings.ollama_base_url.clone();
                self.think.model = settings.ollama_model.clone();
            }
            SummarizerBackend::OpenAICompatible => {
                self.think.base_url = settings.openai_base_url.clone();
                self.think.model = settings.openai_model.clone();
            }
            _ => {}
        }
        if !settings.think_api_key.is_empty() {
            self.think.api_key = settings.think_api_key.clone();
        }
        self.think.disable_local_llama_server = settings.disable_local_llama_server;
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let toml_text = toml::to_string_pretty(self).context("failed to serialize config TOML")?;
        write_atomic(path, toml_text.as_bytes())
            .with_context(|| format!("could not write config file {}", path.display()))
    }

    pub fn export_toml(&self, redact_keys: bool) -> Result<String> {
        let mut copy = self.clone();
        if redact_keys {
            if !copy.transcription.post_correction.api_key.trim().is_empty() {
                copy.transcription.post_correction.api_key = API_KEY_MASK.into();
            }
            if !copy.think.api_key.trim().is_empty() {
                copy.think.api_key = API_KEY_MASK.into();
            }
        }
        toml::to_string_pretty(&copy).context("failed to serialize exported config TOML")
    }

    pub fn import_toml(raw: &str, existing: Option<&ConfigProfile>) -> Result<Self> {
        let mut profile: Self =
            toml::from_str(raw).context("could not parse imported TOML config")?;
        if is_masked_api_key(&profile.transcription.post_correction.api_key) {
            profile.transcription.post_correction.api_key = existing
                .map(|e| e.transcription.post_correction.api_key.clone())
                .unwrap_or_default();
        }
        if is_masked_api_key(&profile.think.api_key) {
            profile.think.api_key = existing
                .map(|e| e.think.api_key.clone())
                .unwrap_or_default();
        }
        profile.validate()?;
        Ok(profile)
    }
}

pub fn load_effective_settings(
    explicit_config: Option<&Path>,
    paths: &LibraryPaths,
) -> Result<(Settings, Option<LoadedConfig>)> {
    let mut settings = Settings::load(&paths.settings_file());
    let loaded = ConfigProfile::resolve_for_app(explicit_config, paths)?;
    if let Some(ref cfg) = loaded {
        cfg.profile.apply_to_settings(&mut settings);
    }
    Ok((settings, loaded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_toml(pc_key: &str, think_key: &str) -> String {
        format!(
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

[transcription.models]
primary = {{ id = "whisper-turbo", device = "igpu" }}
tr_backup = {{ id = "whisper-turbo-tr-khan", device = "igpu" }}
fast_helper = {{ id = "zipformer-oayk", device = "cpu" }}

[transcription.post_correction]
enabled = true
base_url = "https://pc-gateway.example.com/v1"
api_key = "{pc_key}"
primary_judge_model = "Qwen3.8-27B"
support_critic_models = ["gemma-4-31B-it", "gpt-oss-120b"]
segments_per_batch = 5
request_timeout_sec = 120
max_concurrent_batches = 2
continue_on_critic_failure = true

[think]
backend = "openai_compatible"
base_url = "https://think-gateway.example.com/v1"
api_key = "{think_key}"
model = "Qwen3.8-27B"
disable_local_llama_server = true
"#
        )
    }

    #[test]
    fn resolution_priority_and_seeding_respects_all_tiers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        let exe_dir = tmp.path().join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let paths = LibraryPaths::new(&root);

        let template_path = exe_dir.join(CONFIG_FILE_NAME);
        std::fs::write(&template_path, sample_toml("tpl-pc-key", "tpl-think-key")).unwrap();

        let seeded = ConfigProfile::resolve_and_load(None, None, &paths, Some(&exe_dir))
            .unwrap()
            .unwrap();
        assert_eq!(seeded.resolution.active_path, paths.config_file());
        assert_eq!(seeded.resolution.seeded_from, Some(template_path));
        assert!(!seeded.resolution.is_explicit);
        assert_eq!(seeded.profile.think.api_key, "tpl-think-key");
        assert!(paths.config_file().is_file());

        let mut user_profile = seeded.profile.clone();
        user_profile.think.api_key = "user-edited-key".into();
        user_profile.save(&paths.config_file()).unwrap();

        let user_loaded = ConfigProfile::resolve_and_load(None, None, &paths, Some(&exe_dir))
            .unwrap()
            .unwrap();
        assert_eq!(user_loaded.profile.think.api_key, "user-edited-key");
        assert_eq!(user_loaded.resolution.seeded_from, None);

        let env_path = tmp.path().join("env.toml");
        std::fs::write(&env_path, sample_toml("env-pc-key", "env-think-key")).unwrap();
        let env_loaded =
            ConfigProfile::resolve_and_load(None, Some(&env_path), &paths, Some(&exe_dir))
                .unwrap()
                .unwrap();
        assert_eq!(env_loaded.profile.think.api_key, "env-think-key");
        assert!(env_loaded.resolution.is_explicit);

        let explicit_path = tmp.path().join("explicit.toml");
        std::fs::write(&explicit_path, sample_toml("cli-pc-key", "cli-think-key")).unwrap();
        let explicit_loaded = ConfigProfile::resolve_and_load(
            Some(&explicit_path),
            Some(&env_path),
            &paths,
            Some(&exe_dir),
        )
        .unwrap()
        .unwrap();
        assert_eq!(explicit_loaded.profile.think.api_key, "cli-think-key");
        assert_eq!(explicit_loaded.resolution.active_path, explicit_path);
    }

    #[test]
    fn explicit_missing_config_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(tmp.path().join("lib"));
        let missing = tmp.path().join("does-not-exist.toml");
        assert!(ConfigProfile::resolve_and_load(Some(&missing), None, &paths, None).is_err());
        assert!(ConfigProfile::resolve_and_load(None, Some(&missing), &paths, None).is_err());
    }

    #[test]
    fn apply_and_bidirectional_sync_preserve_flags_and_keys() {
        let profile =
            ConfigProfile::import_toml(&sample_toml("gw-pc-secret-42", "gw-think-secret-99"), None)
                .unwrap();
        let mut settings = Settings::default();
        profile.apply_to_settings(&mut settings);

        assert!(settings.onboarding_completed);
        assert_eq!(
            settings.transcription_mode,
            TranscriptionMode::AsymmetricMultiStt
        );
        assert_eq!(settings.transcription_language, "tr");
        assert!(!settings.live_preview_during_meeting);
        assert!(settings.transcription_save_audit_log);
        assert_eq!(settings.transcription_cpu_threads, 6);
        assert!(settings.transcription_auto_fallback_to_cpu);
        assert_eq!(
            settings.summarizer_backend,
            SummarizerBackend::OpenAICompatible
        );
        assert_eq!(
            settings.openai_base_url,
            "https://think-gateway.example.com/v1"
        );
        assert_eq!(settings.openai_model, "Qwen3.8-27B");
        assert!(settings.disable_local_llama_server);
        assert!(
            settings
                .approved_remote_inference_origins
                .contains(&"https://think-gateway.example.com".to_string())
        );
        assert!(
            settings
                .approved_remote_inference_origins
                .contains(&"https://pc-gateway.example.com".to_string())
        );

        let redacted_log =
            crate::text::redact("Auth failed for gw-think-secret-99 and gw-pc-secret-42");
        assert!(!redacted_log.contains("gw-think-secret-99"));
        assert!(!redacted_log.contains("gw-pc-secret-42"));

        settings.transcription_language = "en".into();
        settings.think_api_key.clear();
        let synced = ConfigProfile::from_settings(&settings, Some(&profile));
        assert_eq!(synced.schema_version, 1);
        assert!(synced.deployment.auto_approve_configured_origins);
        assert!(synced.deployment.skip_onboarding_wizard);
        assert_eq!(synced.transcription.language, "en");
        assert_eq!(synced.think.api_key, "gw-think-secret-99");
        assert_eq!(
            synced.transcription.post_correction.api_key,
            "gw-pc-secret-42"
        );
    }

    #[test]
    fn auto_approve_false_leaves_origins_unapproved_and_http_remains_blocked() {
        let mut profile = ConfigProfile::import_toml(&sample_toml("k1", "k2"), None).unwrap();
        profile.deployment.auto_approve_configured_origins = false;
        let mut settings = Settings::default();
        profile.apply_to_settings(&mut settings);
        assert!(settings.approved_remote_inference_origins.is_empty());

        profile.deployment.auto_approve_configured_origins = true;
        profile.think.base_url = "http://remote.example.com/v1".into();
        profile.apply_to_settings(&mut settings);
        assert!(
            !settings
                .approved_remote_inference_origins
                .iter()
                .any(|o| o.starts_with("http://"))
        );
        assert!(
            crate::access::remote_block_reason(
                &settings.openai_base_url,
                &settings.approved_remote_inference_origins
            )
            .is_some()
        );
    }

    #[test]
    fn export_masks_keys_import_preserves_and_validates() {
        let profile =
            ConfigProfile::import_toml(&sample_toml("real-pc-key", "real-think-key"), None)
                .unwrap();
        let exported = profile.export_toml(true).unwrap();
        assert!(!exported.contains("real-pc-key"));
        assert!(!exported.contains("real-think-key"));
        assert!(exported.contains(API_KEY_MASK));

        let reimported = ConfigProfile::import_toml(&exported, Some(&profile)).unwrap();
        assert_eq!(
            reimported.transcription.post_correction.api_key,
            "real-pc-key"
        );
        assert_eq!(reimported.think.api_key, "real-think-key");

        let bad_version = sample_toml("a", "b").replace("schema_version = 1", "schema_version = 2");
        assert!(ConfigProfile::import_toml(&bad_version, None).is_err());

        let bad_url = sample_toml("a", "b").replace(
            "https://think-gateway.example.com/v1",
            "ftp://bad.example.com",
        );
        assert!(ConfigProfile::import_toml(&bad_url, None).is_err());

        let bad_batch =
            sample_toml("a", "b").replace("segments_per_batch = 5", "segments_per_batch = 0");
        assert!(ConfigProfile::import_toml(&bad_batch, None).is_err());
    }

    #[test]
    fn debug_redacts_api_keys() {
        let profile =
            ConfigProfile::import_toml(&sample_toml("secret-pc-xyz", "secret-think-xyz"), None)
                .unwrap();
        let dbg = format!("{profile:?}");
        assert!(!dbg.contains("secret-pc-xyz"));
        assert!(!dbg.contains("secret-think-xyz"));
        assert!(dbg.contains(API_KEY_MASK));
    }

    #[test]
    fn packaged_windows_template_parses_and_validates() {
        let raw = include_str!("../../../packaging/windows/lokalbot-config.toml");
        let profile = ConfigProfile::import_toml(raw, None).unwrap();
        assert_eq!(profile.schema_version, 1);
        assert!(profile.deployment.auto_approve_configured_origins);
        assert!(profile.deployment.skip_onboarding_wizard);
        assert_eq!(
            profile.transcription.mode,
            TranscriptionMode::AsymmetricMultiStt
        );
        assert_eq!(profile.transcription.models.primary.id, "whisper-turbo");
        assert_eq!(
            profile.transcription.models.tr_backup.id,
            "whisper-turbo-tr-khan"
        );
        assert_eq!(
            profile.transcription.models.fast_helper.id,
            "zipformer-oayk"
        );
        assert!(profile.think.disable_local_llama_server);
    }

    #[test]
    fn load_from_path_strips_masked_api_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("masked.toml");
        let toml = sample_toml(API_KEY_MASK, API_KEY_MASK);
        std::fs::write(&path, toml).unwrap();
        let loaded = ConfigProfile::load_from_path(&path).unwrap();
        assert!(loaded.transcription.post_correction.api_key.is_empty());
        assert!(loaded.think.api_key.is_empty());
    }

    #[test]
    fn load_effective_settings_respects_storage_root_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(tmp.path().join("isolated-root"));
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(
            paths.config_file(),
            sample_toml("iso-pc-key", "iso-think-key"),
        )
        .unwrap();

        let (settings, loaded) = load_effective_settings(None, &paths).unwrap();
        let loaded = loaded.unwrap();
        assert_eq!(loaded.resolution.active_path, paths.config_file());
        assert!(settings.onboarding_completed);
        assert_eq!(settings.think_api_key, "iso-think-key");
        assert_eq!(settings.post_correction_api_key, "iso-pc-key");
    }

    #[test]
    fn resolve_and_load_seeding_strips_masked_api_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        let exe_dir = tmp.path().join("app");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let paths = LibraryPaths::new(&root);

        let template_path = exe_dir.join(CONFIG_FILE_NAME);
        std::fs::write(&template_path, sample_toml(API_KEY_MASK, API_KEY_MASK)).unwrap();

        let seeded = ConfigProfile::resolve_and_load(None, None, &paths, Some(&exe_dir))
            .unwrap()
            .unwrap();
        assert!(seeded.profile.think.api_key.is_empty());
        assert!(
            seeded
                .profile
                .transcription
                .post_correction
                .api_key
                .is_empty()
        );
        let written = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(!written.contains(API_KEY_MASK));
        let reloaded = ConfigProfile::load_from_path(&paths.config_file()).unwrap();
        assert!(reloaded.think.api_key.is_empty());
        assert!(reloaded.transcription.post_correction.api_key.is_empty());
    }

    #[test]
    fn apply_to_settings_clears_stale_other_overrides() {
        let profile =
            ConfigProfile::import_toml(&sample_toml("fresh-pc-key", "fresh-think-key"), None)
                .unwrap();
        let mut settings = Settings::default();
        settings.other.insert(
            "postCorrection".into(),
            serde_json::json!({ "enabled": false, "baseUrl": "http://127.0.0.1:9/stale" }),
        );
        settings.other.insert(
            "post_correction".into(),
            serde_json::json!({ "enabled": false }),
        );
        settings
            .other
            .insert("saveAuditLog".into(), serde_json::json!(false));
        settings
            .other
            .insert("save_audit_log".into(), serde_json::json!(false));
        settings
            .other
            .insert("disableLocalLlamaServer".into(), serde_json::json!(false));
        settings.other.insert(
            "disable_local_llama_server".into(),
            serde_json::json!(false),
        );
        settings
            .other
            .insert("thinkApiKey".into(), serde_json::json!("stale-key"));
        settings
            .other
            .insert("think_api_key".into(), serde_json::json!("stale-key-2"));
        settings
            .other
            .insert("multiStt".into(), serde_json::json!({ "enabled": false }));
        settings.other.insert(
            "transcription".into(),
            serde_json::json!({
                "mode": "single",
                "saveAuditLog": false,
                "postCorrection": { "enabled": false },
                "unrelatedNote": "keep"
            }),
        );
        settings.other.insert(
            "think".into(),
            serde_json::json!({
                "api_key": "stale-nested",
                "disable_local_llama_server": false
            }),
        );
        settings
            .other
            .insert("customFlag".into(), serde_json::json!(true));

        profile.apply_to_settings(&mut settings);

        assert!(!settings.other.contains_key("postCorrection"));
        assert!(!settings.other.contains_key("post_correction"));
        assert!(!settings.other.contains_key("saveAuditLog"));
        assert!(!settings.other.contains_key("save_audit_log"));
        assert!(!settings.other.contains_key("disableLocalLlamaServer"));
        assert!(!settings.other.contains_key("disable_local_llama_server"));
        assert!(!settings.other.contains_key("thinkApiKey"));
        assert!(!settings.other.contains_key("think_api_key"));
        assert!(!settings.other.contains_key("multiStt"));
        assert!(!settings.other.contains_key("think"));
        assert_eq!(
            settings.other.get("customFlag"),
            Some(&serde_json::json!(true))
        );
        let tx = settings
            .other
            .get("transcription")
            .and_then(serde_json::Value::as_object)
            .unwrap();
        assert!(!tx.contains_key("mode"));
        assert!(!tx.contains_key("saveAuditLog"));
        assert!(!tx.contains_key("postCorrection"));
        assert_eq!(tx.get("unrelatedNote"), Some(&serde_json::json!("keep")));
    }
}
