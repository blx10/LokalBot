use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use lokalbot_core::LibraryPaths;
use parking_lot::Mutex;
use sherpa_onnx::{
    OfflineNemoEncDecCtcModelConfig, OfflineQwen3ASRModelConfig, OfflineRecognizer,
    OfflineRecognizerConfig, OfflineSenseVoiceModelConfig, OfflineTransducerModelConfig,
    OfflineWhisperModelConfig,
};

use crate::models::{self, Family, SpeechModel, SpeechModelPaths, resolve_model_file};
use crate::whisper_runner::{ComputeDevice, DeviceProvenance, RuntimeLoadConfig, WhisperRunner};

const ZIPFORMER_TAIL_PAD_SAMPLES: usize = 4_800;

pub fn max_span_seconds(family: Family) -> f64 {
    match family {
        Family::Qwen3Asr => 15.0,
        Family::NemoTransducer
        | Family::ZipformerTransducer
        | Family::SenseVoice
        | Family::NemoCtc => 30.0,
        Family::Whisper | Family::WhisperGgml => 28.0,
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decoded {
    pub text: String,
    pub tokens: Vec<String>,
    pub timestamps: Vec<f32>,
    pub words: Vec<lokalbot_core::remote_transcription::TimedWord>,
}

pub fn qwen_language_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "zh" => "Chinese",
        "en" => "English",
        "yue" => "Cantonese",
        "ar" => "Arabic",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "id" => "Indonesian",
        "it" => "Italian",
        "ko" => "Korean",
        "ru" => "Russian",
        "th" => "Thai",
        "vi" => "Vietnamese",
        "ja" => "Japanese",
        "tr" => "Turkish",
        "hi" => "Hindi",
        "ms" => "Malay",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "da" => "Danish",
        "fi" => "Finnish",
        "pl" => "Polish",
        "cs" => "Czech",
        "fil" => "Filipino",
        "fa" => "Persian",
        "el" => "Greek",
        "hu" => "Hungarian",
        "mk" => "Macedonian",
        "ro" => "Romanian",
        "sr" => "Serbian",
        _ => return None,
    })
}

fn qwen_max_new_tokens(samples: usize) -> i32 {
    let seconds = samples as f64 / 16_000.0;
    ((seconds * 18.0) as i32).clamp(128, 768)
}

fn path(file: impl AsRef<Path>) -> Option<String> {
    Some(file.as_ref().to_string_lossy().into_owned())
}

fn c_safe(value: &str) -> String {
    value.replace('\0', "")
}

pub(crate) fn build_sherpa_config(
    paths: &LibraryPaths,
    resolver: &SpeechModelPaths,
    model: &'static SpeechModel,
    threads: usize,
) -> Result<OfflineRecognizerConfig> {
    let family = model
        .family()
        .ok_or_else(|| anyhow!("{} is not a transcription model", model.display_name))?;
    let file = |name: &str| path(resolve_model_file(paths, resolver, model, name));
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.num_threads = threads.clamp(1, 64) as i32;
    config.model_config.provider = Some("cpu".into());
    match family {
        Family::Qwen3Asr => {
            config.model_config.qwen3_asr = OfflineQwen3ASRModelConfig {
                conv_frontend: file("conv_frontend.onnx"),
                encoder: file("encoder.int8.onnx"),
                decoder: file("decoder.int8.onnx"),
                tokenizer: path(resolve_model_file(paths, resolver, model, "tokenizer")),
                max_total_len: 1024,
                max_new_tokens: 512,
                ..Default::default()
            };
            config.model_config.tokens = Some(String::new());
        }
        Family::NemoTransducer => {
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: file("encoder.int8.onnx"),
                decoder: file("decoder.int8.onnx"),
                joiner: file("joiner.int8.onnx"),
            };
            config.model_config.tokens = file("tokens.txt");
            config.model_config.model_type = Some("nemo_transducer".into());
            config.decoding_method = Some("greedy_search".into());
        }
        Family::ZipformerTransducer => {
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: file("encoder.int8.onnx"),
                decoder: file("decoder.int8.onnx"),
                joiner: file("joiner.int8.onnx"),
            };
            config.model_config.tokens = file("tokens.txt");
            config.model_config.model_type = None;
            config.decoding_method = Some("greedy_search".into());
        }
        Family::SenseVoice => {
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: file("model.int8.onnx"),
                language: Some("auto".into()),
                use_itn: true,
            };
            config.model_config.tokens = file("tokens.txt");
        }
        Family::Whisper => {
            config.model_config.whisper = OfflineWhisperModelConfig {
                encoder: file("turbo-encoder.int8.onnx"),
                decoder: file("turbo-decoder.int8.onnx"),
                language: Some(String::new()),
                task: Some("transcribe".into()),
                tail_paddings: -1,
                ..Default::default()
            };
            config.model_config.tokens = file("turbo-tokens.txt");
        }
        Family::NemoCtc => {
            config.model_config.nemo_ctc = OfflineNemoEncDecCtcModelConfig {
                model: file("model.int8.onnx"),
            };
            config.model_config.tokens = file("tokens.txt");
            config.model_config.model_type = Some("nemo_ctc".into());
        }
        Family::WhisperGgml => {
            bail!("{} uses whisper-server rather than sherpa-onnx", model.id);
        }
    }
    Ok(config)
}

enum AsrInner {
    Sherpa {
        model: &'static SpeechModel,
        family: Family,
        base: OfflineRecognizerConfig,
        current: Mutex<(Option<String>, Arc<OfflineRecognizer>)>,
    },
    WhisperCpp {
        family: Family,
        runner: WhisperRunner,
    },
}

pub struct Asr {
    inner: AsrInner,
}

impl Asr {
    #[allow(dead_code)]
    pub fn load(paths: &LibraryPaths, model: &'static SpeechModel, threads: usize) -> Result<Self> {
        Self::load_with_config(
            paths,
            model,
            None,
            RuntimeLoadConfig {
                threads,
                ..Default::default()
            },
        )
    }

    pub fn load_with_config(
        paths: &LibraryPaths,
        model: &'static SpeechModel,
        language: Option<&str>,
        config: RuntimeLoadConfig,
    ) -> Result<Self> {
        let family = model
            .family()
            .ok_or_else(|| anyhow!("{} is not a transcription model", model.display_name))?;
        if family == Family::WhisperGgml {
            let runner = WhisperRunner::load(paths, model, language, config)?;
            return Ok(Self {
                inner: AsrInner::WhisperCpp { family, runner },
            });
        }
        if model.bundled || !model.file_sha256s.is_empty() {
            let dir = models::resolve_model_dir(paths, &config.models_paths, model);
            models::verify_integrity(&dir, model)?;
        }
        let sherpa_cfg =
            build_sherpa_config(paths, &config.models_paths, model, config.threads)?;
        let recognizer = create(&sherpa_cfg, model)?;
        Ok(Self {
            inner: AsrInner::Sherpa {
                model,
                family,
                base: sherpa_cfg,
                current: Mutex::new((None, Arc::new(recognizer))),
            },
        })
    }

    pub fn family(&self) -> Family {
        match &self.inner {
            AsrInner::Sherpa { family, .. } | AsrInner::WhisperCpp { family, .. } => *family,
        }
    }

    pub fn provenance(&self) -> DeviceProvenance {
        match &self.inner {
            AsrInner::WhisperCpp { runner, .. } => runner.provenance(),
            AsrInner::Sherpa { model, .. } => DeviceProvenance {
                model_id: model.id.to_string(),
                requested_device: ComputeDevice::Cpu,
                actual_device: ComputeDevice::Cpu,
                fallback_occurred: false,
                fallback_reason: None,
                backend_detail: "sherpa-onnx-cpu".to_string(),
            },
        }
    }

    fn recognizer(&self, language: Option<&str>) -> Result<Arc<OfflineRecognizer>> {
        let AsrInner::Sherpa {
            model,
            family,
            base,
            current,
        } = &self.inner
        else {
            bail!("not a sherpa-onnx recognizer");
        };
        let fixed_language = match family {
            Family::SenseVoice => language
                .filter(|l| model.supports_language(l))
                .map(str::to_string),
            Family::Whisper => language.map(str::to_string),
            Family::Qwen3Asr
            | Family::NemoTransducer
            | Family::ZipformerTransducer
            | Family::NemoCtc
            | Family::WhisperGgml => None,
        };
        let mut guard = current.lock();
        if guard.0 == fixed_language {
            return Ok(Arc::clone(&guard.1));
        }
        let mut config = base.clone();
        let wanted = fixed_language.clone().unwrap_or_default();
        match family {
            Family::SenseVoice => {
                config.model_config.sense_voice.language = Some(if wanted.is_empty() {
                    "auto".into()
                } else {
                    wanted
                });
            }
            Family::Whisper => config.model_config.whisper.language = Some(wanted),
            _ => {}
        }
        let recognizer = Arc::new(create(&config, model)?);
        *guard = (fixed_language, Arc::clone(&recognizer));
        Ok(recognizer)
    }

    pub fn decode(
        &self,
        samples: &[f32],
        language: Option<&str>,
        hotwords: &[String],
    ) -> Result<Decoded> {
        if samples.is_empty() {
            return Ok(Decoded::default());
        }
        if let AsrInner::WhisperCpp { runner, .. } = &self.inner {
            return runner.decode(samples, language);
        }
        let decoded = self.decode_once(samples, language, hotwords)?;
        if self.family() != Family::NemoTransducer {
            return Ok(decoded);
        }
        let duration = samples.len() as f32 / 16_000.0;
        let last = decoded.timestamps.last().copied().unwrap_or(0.0);
        if duration <= 2.0 || last + 1.5 >= duration {
            return Ok(decoded);
        }
        const LEAD: usize = 4_800;
        let mut padded = vec![0.0f32; LEAD];
        padded.extend_from_slice(samples);
        let mut retry = self.decode_once(&padded, language, hotwords)?;
        if retry.text.trim().chars().count() <= decoded.text.trim().chars().count() {
            return Ok(decoded);
        }
        let lead = LEAD as f32 / 16_000.0;
        for stamp in &mut retry.timestamps {
            *stamp = (*stamp - lead).max(0.0);
        }
        log::debug!(
            "speech: transducer retry recovered {:?} (was {:?})",
            retry.text,
            decoded.text
        );
        Ok(retry)
    }

    fn decode_once(
        &self,
        samples: &[f32],
        language: Option<&str>,
        hotwords: &[String],
    ) -> Result<Decoded> {
        let AsrInner::Sherpa { model, family, .. } = &self.inner else {
            bail!("not a sherpa-onnx recognizer");
        };
        let recognizer = self.recognizer(language)?;
        let stream = recognizer.create_stream();
        if *family == Family::Qwen3Asr {
            stream.set_option(
                "max_new_tokens",
                &qwen_max_new_tokens(samples.len()).to_string(),
            );
            if let Some(name) = language.and_then(qwen_language_name) {
                stream.set_option("language", name);
            }
            if !hotwords.is_empty() {
                let list = hotwords
                    .iter()
                    .map(|h| c_safe(h).replace(',', " "))
                    .collect::<Vec<_>>()
                    .join(",");
                stream.set_option("hotwords", &list);
            }
        }
        let minimum = 1_600;
        if *family == Family::ZipformerTransducer {
            let mut padded = Vec::with_capacity(samples.len() + ZIPFORMER_TAIL_PAD_SAMPLES);
            padded.extend_from_slice(samples);
            padded.resize(
                (samples.len() + ZIPFORMER_TAIL_PAD_SAMPLES).max(minimum),
                0.0,
            );
            stream.accept_waveform(16_000, &padded);
        } else if samples.len() < minimum {
            let mut padded = samples.to_vec();
            padded.resize(minimum, 0.0);
            stream.accept_waveform(16_000, &padded);
        } else {
            stream.accept_waveform(16_000, samples);
        }
        recognizer.decode(&stream);
        let result = stream
            .get_result()
            .ok_or_else(|| anyhow!("{} returned no result", model.display_name))?;
        Ok(Decoded {
            words: Vec::new(),
            text: result.text,
            tokens: result.tokens,
            timestamps: result.timestamps.unwrap_or_default(),
        })
    }
}

fn create(config: &OfflineRecognizerConfig, model: &SpeechModel) -> Result<OfflineRecognizer> {
    match OfflineRecognizer::create(config) {
        Some(recognizer) => Ok(recognizer),
        None => bail!(
            "Could not load {}. Download it again in Settings → Models.",
            model.display_name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_settings() {
        assert_eq!(qwen_language_name("de"), Some("German"));
        assert_eq!(qwen_language_name("sr"), Some("Serbian"));
        assert_eq!(qwen_language_name("uk"), None);
        assert_eq!(qwen_max_new_tokens(16_000), 128);
        assert_eq!(qwen_max_new_tokens(16_000 * 15), 270);
        assert_eq!(qwen_max_new_tokens(16_000 * 60), 768);
    }

    #[test]
    fn zipformer_transducer_config_omits_nemo_model_type() {
        let dir = tempfile::tempdir().unwrap();
        let paths = LibraryPaths::new(dir.path());
        let model = models::find(models::ZIPFORMER_OAYK_MODEL).unwrap();
        let cfg = build_sherpa_config(&paths, &SpeechModelPaths::default(), model, 4).unwrap();
        assert_eq!(cfg.model_config.provider.as_deref(), Some("cpu"));
        assert_eq!(cfg.decoding_method.as_deref(), Some("greedy_search"));
        assert_eq!(cfg.model_config.model_type, None);
        assert!(
            cfg.model_config
                .transducer
                .encoder
                .as_deref()
                .is_some_and(|p| p.ends_with("encoder.int8.onnx"))
        );
        assert!(
            cfg.model_config
                .transducer
                .decoder
                .as_deref()
                .is_some_and(|p| p.ends_with("decoder.int8.onnx"))
        );
        assert!(
            cfg.model_config
                .transducer
                .joiner
                .as_deref()
                .is_some_and(|p| p.ends_with("joiner.int8.onnx"))
        );
        assert!(
            cfg.model_config
                .tokens
                .as_deref()
                .is_some_and(|p| p.ends_with("tokens.txt"))
        );
    }
}
