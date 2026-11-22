use super::*;
use crate::model::{
    AttributionMethod, AttributionSource, Meeting, Segment, SpeakerAttribution, SpeakerIdentity,
    Transcript,
};
use crate::{Library, LibraryPaths};

const KEY: [u8; 32] = [7; 32];

fn library() -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open(LibraryPaths::new(dir.path().to_path_buf())).unwrap();
    (dir, library)
}

fn memory(library: &Library) -> VoiceMemory {
    VoiceMemory::new(library.clone(), Box::new(|| Ok(KEY)), |text| {
        text.contains("sk-secret")
    })
}

fn meeting(library: &Library) -> Meeting {
    let mut meeting = library
        .create_meeting("Call", "Zoom", crate::time::now_seconds())
        .unwrap();
    meeting.ended_at = Some(crate::time::now_seconds());
    library.save_meeting(&meeting).unwrap();
    meeting
}

fn voice(axis: usize, seed: usize) -> Vec<f32> {
    let mut vector = vec![0f32; VOICE_DIMENSION];
    vector[axis] = 1.0;
    vector[(axis + 1 + seed) % VOICE_DIMENSION] = 0.05;
    vector
}

fn sample(
    speaker: &str,
    start: f64,
    axis: usize,
    seed: usize,
    source: AttributionSource,
) -> VoiceSample {
    VoiceSample {
        speaker: speaker.into(),
        range: TurnAnchor {
            start,
            end: start + 6.0,
        },
        vector: voice(axis, seed),
        source: Some(source),
        model: VOICE_MODEL.into(),
    }
}

fn call_samples(speaker: &str, axis: usize) -> Vec<VoiceSample> {
    (0..4)
        .map(|i| sample(speaker, i as f64 * 7.0, axis, i, AttributionSource::System))
        .collect()
}

fn segment(start: f64, end: f64, speaker: &str, source: AttributionSource) -> Segment {
    Segment {
        start,
        end,
        speaker: speaker.into(),
        text: "words said here".into(),
        confidence: None,
        timing_precision: None,
        attribution: Some(SpeakerAttribution {
            source,
            identity: if source == AttributionSource::System {
                SpeakerIdentity::Other
            } else {
                SpeakerIdentity::Unresolved
            },
            method: AttributionMethod::Diarization,
        }),
    }
}

fn call_transcript(speaker: &str) -> Transcript {
    Transcript {
        segments: (0..4)
            .map(|i| {
                let start = i as f64 * 7.0;
                segment(start, start + 6.0, speaker, AttributionSource::System)
            })
            .collect(),
        engine: "test".into(),
        ..Default::default()
    }
}

fn evidence(samples: Vec<VoiceSample>) -> MicrophoneEvidence {
    MicrophoneEvidence {
        samples,
        counts: Default::default(),
    }
}

fn process(
    memory: &VoiceMemory,
    meeting: &Meeting,
    transcript: &Transcript,
    samples: Vec<VoiceSample>,
) -> Transcript {
    let turns: Vec<AudioTurn> = transcript
        .segments
        .iter()
        .map(|s| AudioTurn {
            speaker: s.speaker.clone(),
            range: TurnAnchor {
                start: s.start,
                end: s.end,
            },
            source: Some(AttributionSource::System),
        })
        .collect();
    memory
        .process(ProcessRequest {
            transcript,
            meeting,
            turns: &turns,
            evidence: evidence(samples),
            mic: None,
            system: None,
            previous: None,
            name_automatically: true,
        })
        .unwrap()
}

#[test]
fn reads_the_macs_state_and_profile_json() {
    let state: IdentityState = serde_json::from_str(
        r#"{"schemaVersion":1,"meetingID":"6F9619FF-8B86-D011-B42D-00C04FC964FF","revision":3,
        "evidenceRevision":1,"audioRevision":"mic:aa|system:bb","transcriptSignature":"ff",
        "timeline":[{"speaker":"them 1","range":{"start":1.5,"end":4.5},"source":"system"}],
        "assignments":[{"id":"7F9619FF-8B86-D011-B42D-00C04FC964FF","label":"them 1","name":"Ana",
        "origin":"profileAutomatic","audioRevision":"mic:aa|system:bb","anchors":[{"start":1.5,"end":4.5}],
        "suppressedNames":["marko"],"automaticDisabled":false,
        "match":{"name":"Ana","participantReference":"X","tier":"automatic","source":"profile",
        "supportSeconds":20.5,"independentTurns":4,"supportFraction":0.8,"contradictionSeconds":0,
        "evidence":[{"start":1.5,"end":4.5}],"profileID":"8F9619FF-8B86-D011-B42D-00C04FC964FF",
        "profileRevision":2,"policyVersion":"call-voiceprint-1"},"isLocalUser":false,"source":"system"}],
        "decisions":[{"id":"9F9619FF-8B86-D011-B42D-00C04FC964FF","speakerID":"7F9619FF-8B86-D011-B42D-00C04FC964FF",
        "action":"confirmOther","sourceRevision":"mic:aa|system:bb","name":"Ana","confirmedAt":800000000.5}],
        "suggestions":{"them 1":[]},"analyzedAt":800000000,"voiceSamples":[{"speaker":"them 1",
        "range":{"start":1.5,"end":4.5},"vector":[0.1,0.2],"source":"system",
        "model":"FluidAudio-0.15.8-87a39df/pyannote-community-1/embedding256/16k-mono-v1"}],
        "microphoneSampleDiagnostics":{"quietReference":2},"callVoicesAnalyzed":1}"#,
    )
    .unwrap();
    assert_eq!(state.assignments[0].origin, Origin::ProfileAutomatic);
    assert_eq!(state.decisions[0].action, DecisionAction::ConfirmOther);
    assert_eq!(
        state.assignments[0].matched.as_ref().unwrap().tier,
        MatchTier::Automatic
    );
    assert_eq!(state.call_voices_analyzed, Some(1));

    let database: ProfileDatabase = serde_json::from_str(
        r#"{"schemaVersion":1,"revision":2,"profiles":[{"id":"8F9619FF-8B86-D011-B42D-00C04FC964FF",
        "name":"Ana","confirmedNames":["Ana"],"revision":1,"contributions":[],
        "model":"FluidAudio-0.15.8-87a39df/pyannote-community-1/embedding256/16k-mono-v1"}],
        "forgottenProfiles":["AF9619FF-8B86-D011-B42D-00C04FC964FF"],"deletedMeetings":[],"revokedDecisionIDs":[]}"#,
    )
    .unwrap();
    let json = serde_json::to_value(&database).unwrap();
    assert_eq!(
        json["forgottenProfiles"][0],
        "AF9619FF-8B86-D011-B42D-00C04FC964FF"
    );
    assert_eq!(json["profiles"][0]["confirmedNames"][0], "Ana");
    let written = serde_json::to_value(IdentityState::new(uuid::Uuid::nil())).unwrap();
    for key in [
        "schemaVersion",
        "meetingID",
        "revision",
        "evidenceRevision",
        "audioRevision",
        "timeline",
        "assignments",
        "decisions",
        "suggestions",
        "analyzedAt",
        "voiceSamples",
    ] {
        assert!(written.get(key).is_some(), "{key}");
    }
    let assignment =
        serde_json::to_value(Assignment::new("them", Origin::Legacy, "r", Vec::new())).unwrap();
    for key in [
        "id",
        "label",
        "origin",
        "audioRevision",
        "anchors",
        "suppressedNames",
        "automaticDisabled",
    ] {
        assert!(assignment.get(key).is_some(), "{key}");
    }
}

#[test]
fn sealed_files_use_the_macs_layout_and_a_foreign_key_is_never_overwritten() {
    let (dir, library) = library();
    let meeting = meeting(&library);
    let store = Store::new(dir.path().to_path_buf(), KEY, |_| false);
    let state = IdentityState::new(meeting.id);
    store.commit(state, &meeting, 0, None, None).unwrap();
    let path = library
        .meeting_dir(&meeting)
        .join(EVIDENCE_FOLDER)
        .join(IDENTITY_FILE);
    let sealed = std::fs::read(&path).unwrap();

    use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
    let cipher = aes_gcm::Aes256Gcm::new(aes_gcm::Key::<aes_gcm::Aes256Gcm>::from_slice(&KEY));
    let (nonce, body) = sealed.split_at(12);
    let aad = format!(
        "/{}/speaker-evidence/identity.sealed",
        meeting.relative_path
    );
    let clear = cipher
        .decrypt(
            aes_gcm::Nonce::from_slice(nonce),
            Payload {
                msg: body,
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&clear).unwrap();
    assert_eq!(json["revision"], 1);

    let foreign = Store::new(dir.path().to_path_buf(), [9; 32], |_| false);
    assert!(matches!(foreign.state(&meeting), Err(Failure::Corrupt)));
    let before = std::fs::read(&path).unwrap();
    let mut proposed = IdentityState::new(meeting.id);
    proposed.revision = 9;
    assert!(foreign.commit(proposed, &meeting, 1, None, None).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let profiles = dir.path().join(PROFILES_FILE);
    store.forget_profile(None).unwrap();
    let before = std::fs::read(&profiles).unwrap();
    assert!(foreign.forget_profile(None).is_err());
    assert!(foreign.revoke(meeting.id, None, true).is_err());
    assert_eq!(std::fs::read(&profiles).unwrap(), before);
}

#[test]
fn an_unavailable_key_touches_nothing() {
    let (dir, library) = library();
    let meeting = meeting(&library);
    let memory = VoiceMemory::new(
        library.clone(),
        Box::new(|| Err("no store".to_string())),
        |_| false,
    );
    assert!(matches!(
        memory.state(&meeting),
        Err(Failure::KeyUnavailable(_))
    ));
    let transcript = call_transcript("them 1");
    assert!(
        memory
            .choose(
                &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
                &meeting,
                &transcript,
            )
            .is_err()
    );
    assert!(!dir.path().join(PROFILES_FILE).exists());
    assert!(!library.meeting_dir(&meeting).join(EVIDENCE_FOLDER).exists());
}

#[test]
fn naming_teaches_a_voice_and_a_later_meeting_is_named_with_undo() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let transcript = call_transcript("them 1");
    let saved = process(&memory, &first, &transcript, call_samples("them 1", 3));
    assert!(saved.speaker_aliases.is_empty(), "nobody is known yet");

    let named = memory
        .choose(
            &Choice::new(
                "them 1",
                Some("Ana Petrović".into()),
                DecisionAction::Assign,
            ),
            &first,
            &saved,
        )
        .unwrap();
    assert_eq!(named.speaker_aliases["them 1"], "Ana Petrović");
    let people = memory.profiles().unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].name, "Ana Petrović");
    assert_eq!(people[0].model, VOICE_MODEL);
    assert_eq!(
        people[0].contributions[0].samples.len(),
        1,
        "one chunk plus the average"
    );
    assert!(people[0].contributions[0].voiceprint.is_some());

    let second = meeting(&library);
    let later = call_transcript("them 2");
    let named_later = process(&memory, &second, &later, call_samples("them 2", 3));
    assert_eq!(named_later.speaker_aliases["them 2"], "Ana Petrović");
    let state = memory.state(&second).unwrap();
    assert_eq!(
        state.assignment("them 2").unwrap().origin,
        Origin::ProfileAutomatic
    );
    assert_eq!(
        memory.profiles().unwrap()[0].contributions.len(),
        1,
        "automatic names never train"
    );

    let undone = memory
        .choose(
            &Choice::new("them 2", Some("Ana Petrović".into()), DecisionAction::Undo),
            &second,
            &named_later,
        )
        .unwrap();
    assert!(!undone.speaker_aliases.contains_key("them 2"));
    let state = memory.state(&second).unwrap();
    let assignment = state.assignment("them 2").unwrap();
    assert!(assignment.automatic_disabled);
    assert!(
        assignment
            .suppressed_names
            .contains(&name_key("Ana Petrović"))
    );
    let again = process(&memory, &second, &undone, call_samples("them 2", 3));
    assert!(
        !again.speaker_aliases.contains_key("them 2"),
        "the undo holds"
    );
}

#[test]
fn a_different_voice_is_not_named_and_a_correction_revokes_the_old_contribution() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    let named = memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();

    let other = meeting(&library);
    let stranger = process(
        &memory,
        &other,
        &call_transcript("them 1"),
        call_samples("them 1", 90),
    );
    assert!(stranger.speaker_aliases.is_empty());

    let corrected = memory
        .choose(
            &Choice::new("them 1", Some("Marko".into()), DecisionAction::Assign),
            &first,
            &named,
        )
        .unwrap();
    assert_eq!(corrected.speaker_aliases["them 1"], "Marko");
    let people = memory.profiles().unwrap();
    assert_eq!(
        people.len(),
        1,
        "the old contribution left with its only profile"
    );
    assert_eq!(people[0].name, "Marko");
    let database = memory.store().unwrap().profiles().unwrap();
    assert_eq!(database.forgotten_profiles.len(), 1);
    assert_eq!(database.revoked_decision_ids.len(), 1);
}

#[test]
fn profiles_of_another_voice_model_are_never_compared_or_added_to() {
    let mut mac_sample = sample("them 1", 0.0, 3, 0, AttributionSource::System);
    mac_sample.model =
        "FluidAudio-0.15.8-87a39df/pyannote-community-1/embedding256/16k-mono-v1".into();
    let mac = VoiceProfile {
        id: uuid::Uuid::new_v4(),
        name: "Ana".into(),
        is_local_user: None,
        confirmed_names: vec!["Ana".into()],
        revision: 1,
        contributions: vec![Contribution {
            meeting_id: uuid::Uuid::new_v4(),
            audio_revision: "r".into(),
            speaker_id: uuid::Uuid::new_v4(),
            decision_id: uuid::Uuid::new_v4(),
            confirmed_at: chrono::Utc::now(),
            samples: vec![mac_sample.clone()],
            voiceprint: Some(voice(3, 0)),
            meeting_title: None,
            meeting_started_at: None,
        }],
        model: mac_sample.model.clone(),
    };
    let ours = call_samples("them 1", 3);
    assert!(matches(&ours, std::slice::from_ref(&mac)).is_empty());
    assert_eq!(same_person("Ana", &ours, std::slice::from_ref(&mac)), None);
    assert!(eligible(&[mac_sample], &[], "them 1").is_empty());
}

#[test]
fn saved_meetings_only_get_suggestions_until_a_person_applies_them() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();

    let second = meeting(&library);
    let transcript = call_transcript("them 1");
    let turns: Vec<AudioTurn> = transcript
        .segments
        .iter()
        .map(|s| AudioTurn {
            speaker: s.speaker.clone(),
            range: TurnAnchor {
                start: s.start,
                end: s.end,
            },
            source: Some(AttributionSource::System),
        })
        .collect();
    let result = memory
        .process(ProcessRequest {
            transcript: &transcript,
            meeting: &second,
            turns: &turns,
            evidence: evidence(call_samples("them 1", 3)),
            mic: None,
            system: None,
            previous: None,
            name_automatically: false,
        })
        .unwrap();
    assert!(
        result.speaker_aliases.is_empty(),
        "a saved transcript is never renamed"
    );
    let state = memory.state(&second).unwrap();
    let recognized = recognized_speakers(Some(&state), &result);
    assert_eq!(recognized.len(), 1);
    assert_eq!(recognized[0].1.name, "Ana");
}

#[test]
fn deleting_a_meeting_preserves_profile_and_metadata() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();
    assert_eq!(memory.profiles().unwrap().len(), 1);
    memory.prepare_deletion(&first).unwrap();
    let profiles = memory.profiles().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(
        profiles[0].contributions[0].meeting_title.as_deref(),
        Some(first.display_title().as_str())
    );
    assert_eq!(
        profiles[0].contributions[0].meeting_started_at,
        Some(first.started_at)
    );
    let database = memory.store().unwrap().profiles().unwrap();
    assert!(database.deleted_meetings.contains(&first.id));
}

#[test]
fn the_sample_meeting_is_never_remembered_or_matched() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let mut sample_meeting = meeting(&library);
    sample_meeting.sample_id = Some("orders-api/en".into());
    library.save_meeting(&sample_meeting).unwrap();
    let saved = process(
        &memory,
        &sample_meeting,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    assert!(
        memory
            .state(&sample_meeting)
            .unwrap()
            .voice_samples
            .is_empty()
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &sample_meeting,
            &saved,
        )
        .unwrap();
    assert!(memory.profiles().unwrap().is_empty());
}

#[test]
fn evidence_expires_but_names_stay() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();
    assert!(!memory.state(&first).unwrap().voice_samples.is_empty());
    memory.expire(
        std::slice::from_ref(&first),
        7,
        chrono::Utc::now() + chrono::Duration::days(8),
    );
    let state = memory.state(&first).unwrap();
    assert!(state.voice_samples.is_empty());
    assert!(state.suggestions.is_empty());
    assert_eq!(state.evidence_revision, 1);
    assert_eq!(
        state.assignment("them 1").unwrap().name.as_deref(),
        Some("Ana")
    );
    assert_eq!(
        memory.profiles().unwrap().len(),
        1,
        "profiles outlive evidence"
    );
}

#[test]
fn enrollment_needs_two_turns_totaling_eight_seconds() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let transcript = Transcript {
        segments: vec![segment(0.0, 6.0, "them 1", AttributionSource::System)],
        engine: "test".into(),
        ..Default::default()
    };
    let saved = process(
        &memory,
        &first,
        &transcript,
        call_samples("them 1", 3).into_iter().take(1).collect(),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();
    assert!(memory.profiles().unwrap().is_empty());
    assert!(
        memory
            .notice()
            .unwrap()
            .contains("two separate clear turns")
    );

    let second = meeting(&library);
    let two_transcript = Transcript {
        segments: (0..2)
            .map(|i| {
                segment(
                    i as f64 * 7.0,
                    i as f64 * 7.0 + 4.0,
                    "them 1",
                    AttributionSource::System,
                )
            })
            .collect(),
        engine: "test".into(),
        ..Default::default()
    };
    let two_samples: Vec<VoiceSample> = (0..2)
        .map(|i| {
            let mut s = sample("them 1", i as f64 * 7.0, 3, i, AttributionSource::System);
            s.range.end = s.range.start + 4.0;
            s
        })
        .collect();
    let saved_two = process(&memory, &second, &two_transcript, two_samples);
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &second,
            &saved_two,
        )
        .unwrap();
    assert_eq!(memory.profiles().unwrap().len(), 1);
}

#[test]
fn matching_rules_follow_the_mac() {
    assert!(can_enroll(&call_samples("them 1", 3)));
    let mut mixed = call_samples("them 1", 3);
    mixed[1].vector = voice(50, 0);
    mixed[2].vector = voice(90, 0);
    assert!(
        !can_enroll(&mixed),
        "a mixed label fails the consistency check"
    );
    assert!(can_enroll(&[
        sample("local 1", 0.0, 4, 0, AttributionSource::Microphone),
        sample("local 1", 7.0, 4, 1, AttributionSource::Microphone),
        sample("local 1", 14.0, 4, 2, AttributionSource::Microphone),
    ]));
    assert_eq!(name_key("  Ana PETROVIĆ "), "ana petrovic");
    let secret = |text: &str| text.contains("sk-secret");
    assert_eq!(
        safe_name(" Ana   Marija ", &secret).as_deref(),
        Some("Ana Marija")
    );
    for bad in [
        "a",
        "you",
        "Everyone",
        "ana@example.com",
        "http://x",
        "my sk-secret",
    ] {
        assert_eq!(safe_name(bad, &secret), None, "{bad}");
    }
}

#[test]
fn a_merge_of_different_people_does_not_inherit_a_name() {
    let mut ana = Assignment::new(
        "them 1",
        Origin::UserConfirmed,
        "rev",
        vec![TurnAnchor {
            start: 0.0,
            end: 10.0,
        }],
    );
    ana.name = Some("Ana".into());
    ana.source = Some(AttributionSource::System);
    let mut marko = Assignment::new(
        "them 2",
        Origin::UserConfirmed,
        "rev",
        vec![TurnAnchor {
            start: 10.0,
            end: 20.0,
        }],
    );
    marko.name = Some("Marko".into());
    marko.source = Some(AttributionSource::System);
    let turn = |speaker: &str, start: f64, end: f64| AudioTurn {
        speaker: speaker.into(),
        range: TurnAnchor { start, end },
        source: Some(AttributionSource::System),
    };
    let previous = [ana, marko];
    let merged = remap(&previous, &[turn("them 1", 0.0, 20.0)], "rev");
    assert!(merged.is_empty());
    let split = remap(
        &previous,
        &[turn("them 3", 0.0, 5.0), turn("them 4", 5.0, 10.0)],
        "rev",
    );
    assert_eq!(split.len(), 2, "a clean split inherits");
    assert!(remap(&previous, &[turn("them 1", 0.0, 10.0)], "other revision").is_empty());
}

#[test]
fn signatures_format_times_like_swift() {
    let transcript = Transcript {
        segments: vec![segment(0.00001, 1.0, "Them 1", AttributionSource::System)],
        ..Default::default()
    };
    use sha2::{Digest as _, Sha256};
    assert_eq!(
        transcript_signature(&transcript),
        hex::encode(Sha256::digest("1e-05|1.0|them 1".as_bytes()))
    );
}

#[test]
fn microphone_samples_need_a_quiet_call_track() {
    let transcript = Transcript {
        segments: vec![segment(0.0, 6.0, "local 1", AttributionSource::Microphone)],
        ..Default::default()
    };
    let samples = vec![sample("local 1", 0.0, 3, 0, AttributionSource::Microphone)];
    let mut quiet = |start: f64, end: f64| Some(vec![0f32; ((end - start) * 16_000.0) as usize]);
    let kept = select_microphone_samples(
        &samples,
        &transcript,
        &[],
        true,
        &TrackClock::SharedTimeline,
        &mut quiet,
    );
    assert_eq!(kept.samples.len(), 1);
    assert_eq!(kept.counts["quietReference"], 1);

    let mut loud = |start: f64, end: f64| Some(vec![0.1f32; ((end - start) * 16_000.0) as usize]);
    let dropped = select_microphone_samples(
        &samples,
        &transcript,
        &[],
        true,
        &TrackClock::SharedTimeline,
        &mut loud,
    );
    assert!(dropped.samples.is_empty());
    assert_eq!(dropped.counts["remoteAudioOrMissingSamples"], 1);

    let mut never = |_: f64, _: f64| None;
    let unknown = select_microphone_samples(
        &samples,
        &transcript,
        &[],
        true,
        &TrackClock::Unknown,
        &mut never,
    );
    assert_eq!(unknown.counts["missingClockOrReference"], 1);
    let alone = select_microphone_samples(
        &samples,
        &transcript,
        &[],
        false,
        &TrackClock::Unknown,
        &mut never,
    );
    assert_eq!(alone.counts["microphoneOnly"], 1);
}

#[test]
fn reads_the_macs_timing_file() {
    let timing: RecordingAudioTiming = serde_json::from_str(
        r#"{"version":2,"microphone":[{"hostStart":100,"hostEnd":110,"startFrame":0,"endFrame":160000,"sampleRate":16000,"generation":0}],
        "system":[{"hostStart":100.5,"hostEnd":110,"startFrame":0,"endFrame":152000,"sampleRate":16000,"generation":0}],"timelineOriginHostTime":99.5}"#,
    )
    .unwrap();
    let range = timing.reference_range(1.0, 2.0).unwrap();
    assert!((range.start - 0.5).abs() < 1e-6 && (range.end - 1.5).abs() < 1e-6);
    assert_eq!(
        timing.reference_range(0.0, 1.0),
        None,
        "no system audio yet"
    );
    assert_eq!(timing.next_boundary(0.0), Some(0.5));
}

#[test]
fn a_removed_meetings_voice_stays_preserved_with_metadata() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let second = meeting(&library);
    for (meeting, axis) in [(&first, 3), (&second, 9)] {
        let saved = process(
            &memory,
            meeting,
            &call_transcript("them 1"),
            call_samples("them 1", axis),
        );
        memory
            .choose(
                &Choice::new(
                    "them 1",
                    Some(format!("Person {axis}")),
                    DecisionAction::Assign,
                ),
                meeting,
                &saved,
            )
            .unwrap();
    }
    assert_eq!(memory.profiles().unwrap().len(), 2);
    memory.revoke_removed_meetings(std::slice::from_ref(&second));
    let people = memory.profiles().unwrap();
    assert_eq!(people.len(), 2);
    assert!(
        memory
            .store()
            .unwrap()
            .profiles()
            .unwrap()
            .deleted_meetings
            .contains(&first.id)
    );
}

#[test]
fn notices_belong_to_the_meeting_they_came_from() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let (first, second) = (meeting(&library), meeting(&library));
    memory.note_unavailable(first.id, &Failure::Corrupt);
    assert!(memory.notice_for(first.id).is_some());
    assert!(memory.notice_for(second.id).is_none());
}

#[test]
fn suggesting_only_leaves_names_to_a_person_and_a_strong_match_is_recognized() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();
    let second = meeting(&library);
    let transcript = call_transcript("them 1");
    let turns: Vec<AudioTurn> = transcript
        .segments
        .iter()
        .map(|s| AudioTurn {
            speaker: s.speaker.clone(),
            range: TurnAnchor {
                start: s.start,
                end: s.end,
            },
            source: Some(AttributionSource::System),
        })
        .collect();
    let named = memory
        .process(ProcessRequest {
            transcript: &transcript,
            meeting: &second,
            turns: &turns,
            evidence: evidence(call_samples("them 1", 3)),
            mic: None,
            system: None,
            previous: None,
            name_automatically: false,
        })
        .unwrap();
    assert!(named.speaker_aliases.is_empty());
    let state = memory.state(&second).unwrap();
    let assignment = state.assignment("them 1").unwrap();
    assert_eq!(assignment.name, None);
    assert_eq!(assignment.origin, Origin::CalendarAutomatic);
    let recognized = recognized_speakers(Some(&state), &named);
    assert_eq!(recognized[0].1.name, "Ana");
    let applied = memory
        .choose(
            &Choice::new("them 1", Some("Ana".into()), DecisionAction::Assign),
            &second,
            &named,
        )
        .unwrap();
    assert_eq!(applied.speaker_aliases["them 1"], "Ana");
    assert_eq!(memory.profiles().unwrap()[0].contributions.len(), 2);
}

#[test]
fn relaxed_suggestions_cross_channel_same_person_and_live_suggestions() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Alper".into()), DecisionAction::Assign),
            &first,
            &saved,
        )
        .unwrap();
    let profiles = memory.profiles().unwrap();
    assert_eq!(profiles.len(), 1);
    let other_voice = call_samples("them 1", 40);
    assert_eq!(
        same_person("alper", &other_voice, &profiles),
        Some(profiles[0].id)
    );
    assert_eq!(same_person("Other", &other_voice, &profiles), None);

    let single = vec![sample("them 1", 0.0, 3, 0, AttributionSource::System)];
    let single_match = matches(&single, &profiles);
    assert_eq!(single_match.len(), 1);
    assert_eq!(single_match[0].tier, MatchTier::Suggested);

    let mut mild = call_samples("them 1", 3);
    for s in &mut mild {
        s.vector = vec![0f32; VOICE_DIMENSION];
        s.vector[3] = 0.50;
        s.vector[10] = 0.866_025_4;
    }
    let mild_match = matches(&mild, &profiles);
    assert_eq!(mild_match.len(), 1);
    assert_eq!(mild_match[0].tier, MatchTier::Suggested);

    let mic_query: Vec<VoiceSample> = call_samples("local 1", 3)
        .into_iter()
        .map(|mut s| {
            s.source = Some(AttributionSource::Microphone);
            s
        })
        .collect();
    assert!(matches(&mic_query, &profiles).is_empty());
    let cross = matches_with_options(&mic_query, &profiles, true);
    assert_eq!(cross.len(), 1);
    assert_eq!(cross[0].tier, MatchTier::Suggested);
    assert_eq!(cross[0].name, "Alper");

    let old_meeting = meeting(&library);
    let old_transcript = call_transcript("them 1");
    let _ = process(
        &memory,
        &old_meeting,
        &old_transcript,
        call_samples("them 1", 3),
    );
    let old_state = memory.state(&old_meeting).unwrap();
    let live = live_suggestions(Some(&old_state), Some(&old_transcript), &profiles);
    assert_eq!(live["them 1"][0].name, "Alper");
}

#[test]
fn merge_profiles_combines_contributions_and_rejects_mismatched_local_user() {
    let (_dir, library) = library();
    let memory = memory(&library);
    let first = meeting(&library);
    let first_saved = process(
        &memory,
        &first,
        &call_transcript("them 1"),
        call_samples("them 1", 3),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ada".into()), DecisionAction::Assign),
            &first,
            &first_saved,
        )
        .unwrap();

    let second = meeting(&library);
    let second_saved = process(
        &memory,
        &second,
        &call_transcript("them 1"),
        call_samples("them 1", 12),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Ada Alt".into()), DecisionAction::Assign),
            &second,
            &second_saved,
        )
        .unwrap();

    let profiles = memory.profiles().unwrap();
    assert_eq!(profiles.len(), 2);
    let target = profiles.iter().find(|p| p.name == "Ada").unwrap();
    let source = profiles.iter().find(|p| p.name == "Ada Alt").unwrap();

    memory.merge_profiles(source.id, target.id).unwrap();
    let after = memory.profiles().unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, target.id);
    assert_eq!(after[0].name, "Ada");
    assert_eq!(after[0].contributions.len(), 2);
    assert!(after[0].confirmed_names.contains(&"Ada".to_string()));
    assert!(after[0].confirmed_names.contains(&"Ada Alt".to_string()));

    let third = meeting(&library);
    let third_saved = process(
        &memory,
        &third,
        &call_transcript("them 1"),
        call_samples("them 1", 25),
    );
    memory
        .choose(
            &Choice::new("them 1", Some("Sam".into()), DecisionAction::ConfirmOther),
            &third,
            &third_saved,
        )
        .unwrap();

    let local = meeting(&library);
    let local_transcript = Transcript {
        segments: (0..4)
            .map(|i| {
                let start = i as f64 * 7.0;
                segment(start, start + 6.0, "me", AttributionSource::Microphone)
            })
            .collect(),
        engine: "test".into(),
        ..Default::default()
    };
    let mic_turns: Vec<AudioTurn> = local_transcript
        .segments
        .iter()
        .map(|s| AudioTurn {
            speaker: s.speaker.clone(),
            range: TurnAnchor {
                start: s.start,
                end: s.end,
            },
            source: Some(AttributionSource::Microphone),
        })
        .collect();
    let mic_samples: Vec<VoiceSample> = (0..4)
        .map(|i| sample("me", i as f64 * 7.0, 20, i, AttributionSource::Microphone))
        .collect();
    let local_saved = memory
        .process(ProcessRequest {
            transcript: &local_transcript,
            meeting: &local,
            turns: &mic_turns,
            evidence: evidence(mic_samples),
            mic: None,
            system: None,
            previous: None,
            name_automatically: true,
        })
        .unwrap();
    let mut confirm = Choice::new("me", Some("Alper".into()), DecisionAction::ConfirmUser);
    confirm.action = DecisionAction::ConfirmUser;
    memory.choose(&confirm, &local, &local_saved).unwrap();
    let all = memory.profiles().unwrap();
    let user_profile = all.iter().find(|p| p.is_local_user == Some(true)).unwrap();
    let other_profile = all.iter().find(|p| p.is_local_user == Some(false)).unwrap();
    assert!(matches!(
        memory.merge_profiles(other_profile.id, user_profile.id),
        Err(Failure::Stale)
    ));
}
