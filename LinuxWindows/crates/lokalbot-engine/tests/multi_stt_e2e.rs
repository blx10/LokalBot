use std::path::{Path, PathBuf};

use lokalbot_core::LibraryPaths;
use lokalbot_engine::{ComputeDevice, MultiSttConfig, SpeechCache};
use lokalbot_speech::TranscribeOptions;

fn find_staging_dir() -> PathBuf {
    if let Some(env_dir) = std::env::var_os("LOKALBOT_MODELS_DIR") {
        let p = PathBuf::from(env_dir);
        if p.exists() {
            return p;
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        let direct = ancestor.join("bundled_models_staging");
        if direct.is_dir() {
            return direct;
        }
        let under_local = ancestor.join("Local").join("bundled_models_staging");
        if under_local.is_dir() {
            return under_local;
        }
    }
    manifest.join("../../../../../Local/bundled_models_staging")
}

#[test]
fn speech_cache_runs_multi_stt_on_short_synthetic_wav() {
    let dir = tempfile::tempdir().unwrap();
    let paths = LibraryPaths::new(dir.path());
    let wav = dir.path().join("silence.wav");
    lokalbot_audio::write_wav_16k(&wav, &vec![0.0f32; 1600]).unwrap();
    let cache = SpeechCache::default();
    let res = cache
        .run_multi_stt(
            &paths,
            &MultiSttConfig::bundled_default(),
            None,
            Some(&wav),
            &TranscribeOptions::default(),
            &mut |_| {},
        )
        .unwrap();
    assert!(res.segments.is_empty());
    assert!(res.transcript.segments.is_empty());
}

#[test]
#[ignore = "requires staged 3-model bundle and örnek2.wav"]
fn e2e_multi_stt_ornek2_wav() {
    let staging = find_staging_dir();
    assert!(
        staging.is_dir(),
        "staging dir not found: {}",
        staging.display()
    );
    let whisper_exe_name = if cfg!(windows) {
        "whisper-server.exe"
    } else {
        "whisper-server"
    };
    let whisper_bin = std::env::var_os("LOKALBOT_WHISPER_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            staging
                .join("runtimes")
                .join("whisper-cpp")
                .join(whisper_exe_name)
        });
    assert!(
        whisper_bin.is_file(),
        "whisper-server binary not found: {}",
        whisper_bin.display()
    );

    let wav_path = Path::new(r"z:\projeler\Turkce-STT-Test\samples\örnek2.wav");
    assert!(wav_path.is_file(), "sample not found: {}", wav_path.display());

    let tmp = tempfile::tempdir().unwrap();
    let paths = LibraryPaths::new(tmp.path());
    let mut cfg = MultiSttConfig::bundled_default();
    cfg.models_dir = Some(staging.clone());
    cfg.whisper_server_bin = Some(whisper_bin);

    let options = TranscribeOptions {
        language: Some("tr".to_string()),
        diarize: true,
        microphone_is_user: false,
        ..Default::default()
    };

    let cache = SpeechCache::default();
    let mut events = Vec::new();
    let result = cache
        .run_multi_stt(&paths, &cfg, None, Some(wav_path), &options, &mut |ev| {
            events.push(format!("{ev:?}"));
        })
        .unwrap();

    assert_eq!(result.model_runs.len(), 3);
    assert_eq!(result.model_runs[0].model_id, "whisper-turbo");
    assert_eq!(result.model_runs[1].model_id, "whisper-turbo-tr-khan");
    assert_eq!(result.model_runs[2].model_id, "zipformer-oayk");
    assert_eq!(
        result.model_runs[2].provenance.actual_device,
        ComputeDevice::Cpu
    );

    for run in &result.model_runs {
        println!(
            "PASS role={} model={} requested={:?} actual={:?} fallback={} detail={} elapsed_ms={} segments={}",
            run.role,
            run.model_id,
            run.provenance.requested_device,
            run.provenance.actual_device,
            run.provenance.fallback_occurred,
            run.provenance.backend_detail,
            run.elapsed_ms,
            run.segments_decoded
        );
    }

    assert!(!result.transcript.segments.is_empty());
    assert_eq!(result.segments.len(), result.transcript.segments.len());

    let mut primary_all = Vec::new();
    let mut backup_all = Vec::new();
    let mut helper_all = Vec::new();
    for (i, (t_seg, m_seg)) in result
        .transcript
        .segments
        .iter()
        .zip(result.segments.iter())
        .enumerate()
    {
        assert_eq!(m_seg.segment_id, i);
        assert_eq!(t_seg.text, m_seg.primary);
        println!(
            "[seg {i} {:.2}->{:.2} {}]\n  primary:     {}\n  tr_backup:   {}\n  fast_helper: {}",
            m_seg.start, m_seg.end, m_seg.speaker, m_seg.primary, m_seg.tr_backup, m_seg.fast_helper
        );
        if !m_seg.primary.is_empty() {
            primary_all.push(m_seg.primary.clone());
        }
        if !m_seg.tr_backup.is_empty() {
            backup_all.push(m_seg.tr_backup.clone());
        }
        if !m_seg.fast_helper.is_empty() {
            helper_all.push(m_seg.fast_helper.clone());
        }
    }

    let p_joined = primary_all.join(" ").to_lowercase();
    let b_joined = backup_all.join(" ").to_lowercase();
    let h_joined = helper_all.join(" ").to_lowercase();
    assert!(!p_joined.is_empty(), "primary produced empty text");
    assert!(!b_joined.is_empty(), "tr_backup produced empty text");
    assert!(!h_joined.is_empty(), "fast_helper produced empty text");
    assert!(
        p_joined.contains("skype")
            || p_joined.contains("windows")
            || p_joined.contains("admin")
            || p_joined.contains("mute"),
        "primary did not contain expected domain words: {p_joined}"
    );
}
