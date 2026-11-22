use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use chrono::Duration;

use crate::library::{
    Library, MIC_TRACK, SYSTEM_TRACK, TRANSCRIPT_FILE, TRANSCRIPT_MD_FILE, write_json,
};
use crate::merge::{Slice, Splice};
use crate::model::{ContentRange, Meeting, Transcript};

pub const MINIMUM_SELECTION_DURATION: f64 = 0.25;

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("Stop the recording before creating a meeting from a selection.")]
    RecordingStillActive,
    #[error("Choose a valid start and end time for the selection.")]
    InvalidRange,
    #[error("This recording has no playable audio duration to extract.")]
    InvalidDuration,
    #[error("No readable audio was found inside the selected range.")]
    NoAudioTracks,
    #[error("Could not create audio for the new meeting: {0}")]
    ExportFailed(String),
}

impl ExtractError {
    pub fn localized(&self) -> String {
        match self {
            Self::RecordingStillActive => {
                lokalbot_i18n::tr("Stop the recording before creating a meeting from a selection.")
                    .into()
            }
            Self::InvalidRange => {
                lokalbot_i18n::tr("Choose a valid start and end time for the selection.").into()
            }
            Self::InvalidDuration => {
                lokalbot_i18n::tr("This recording has no playable audio duration to extract.")
                    .into()
            }
            Self::NoAudioTracks => {
                lokalbot_i18n::tr("No readable audio was found inside the selected range.").into()
            }
            Self::ExportFailed(reason) => lokalbot_i18n::format(
                "Could not create audio for the new meeting: {reason}",
                &[("reason", reason.clone())],
            ),
        }
    }
}

pub fn extracted_title(source: &Meeting, start: f64, end: f64) -> String {
    format!(
        "{} ({}–{})",
        source.display_title(),
        crate::time::short_stamp(start),
        crate::time::short_stamp(end)
    )
}

pub fn extract_meeting_from_selection(
    library: &Library,
    source: &Meeting,
    range: ContentRange,
    audio_duration: Option<f64>,
    splice: &Splice<'_>,
) -> Result<Meeting, ExtractError> {
    if library.is_read_only() {
        return Err(ExtractError::ExportFailed(
            "the library is open read-only".into(),
        ));
    }
    if source.is_live() {
        return Err(ExtractError::RecordingStillActive);
    }
    if !range.is_valid() {
        return Err(ExtractError::InvalidRange);
    }
    let raw_duration = source
        .recorded_duration
        .unwrap_or(0.0)
        .max(audio_duration.unwrap_or(0.0))
        .max(source.duration_seconds());
    if !(raw_duration.is_finite() && raw_duration > 0.0) {
        return Err(ExtractError::InvalidDuration);
    }
    let start = range.start.clamp(0.0, raw_duration);
    let end = range.end.max(start).min(raw_duration);
    let slice_duration = end - start;
    if slice_duration < MINIMUM_SELECTION_DURATION {
        return Err(ExtractError::InvalidRange);
    }

    let title = extracted_title(source, start, end);
    let started_at = source.started_plus(start);
    let ended_at = started_at + Duration::milliseconds((slice_duration * 1000.0).round() as i64);
    let relative = library
        .reserve_meeting_folder(&title, started_at)
        .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?;
    let dest_folder = library.paths().meeting_dir(&relative);

    let result = build_extracted(
        library,
        source,
        &dest_folder,
        &relative,
        &title,
        started_at,
        ended_at,
        start,
        end,
        slice_duration,
        splice,
    );
    if result.is_err() {
        let _ = fs::remove_dir_all(&dest_folder);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn build_extracted(
    library: &Library,
    source: &Meeting,
    dest_folder: &Path,
    relative: &str,
    title: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
    start: f64,
    end: f64,
    slice_duration: f64,
    splice: &Splice<'_>,
) -> Result<Meeting, ExtractError> {
    let audio = library.audio(source);
    let mut tracks = [false, false];
    for (index, (src, name)) in [(audio.mic, MIC_TRACK), (audio.system, SYSTEM_TRACK)]
        .into_iter()
        .enumerate()
    {
        if src.is_some() {
            let slice = Slice {
                source: src,
                start,
                end,
            };
            let used = splice(&[slice], &dest_folder.join(name))
                .map_err(|e| ExtractError::ExportFailed(e.to_string()))?;
            if used.first().copied().unwrap_or(false) {
                tracks[index] = true;
            }
        }
    }
    if !tracks[0] && !tracks[1] {
        return Err(ExtractError::NoAudioTracks);
    }

    if let Some(sliced) = slice_transcript(library, source, start, end, slice_duration)? {
        write_json(&dest_folder.join(TRANSCRIPT_FILE), &sliced)
            .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?;
        crate::library::write_atomic(
            &dest_folder.join(TRANSCRIPT_MD_FILE),
            sliced.markdown().as_bytes(),
        )
        .map_err(|e| ExtractError::ExportFailed(e.to_string()))?;
    }

    let notes = library
        .notes(source)
        .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?;

    let mut created = Meeting::new(title, &source.app_name, started_at, relative);
    created.ended_at = Some(ended_at);
    created.has_system_track = tracks[1];
    created.participant_name_hints = source.participant_name_hints.clone();
    created.calendar_participant_identities = source.calendar_participant_identities.clone();
    created.extracted_from_meeting_id = Some(source.id);
    created.imported_file_names = source.imported_file_names.clone();
    created.recorded_duration = Some(slice_duration);
    created.calendar_event_id = None;
    created.calendar_title = None;
    if !notes.trim().is_empty() {
        library
            .save_notes(&created, &notes)
            .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?;
    }
    crate::evidence::with_meeting_mutation(library.paths().root(), &[&created], || {
        library.save_meeting(&created)
    })
    .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?;
    Ok(created)
}

fn slice_transcript(
    library: &Library,
    source: &Meeting,
    start: f64,
    end: f64,
    slice_duration: f64,
) -> Result<Option<Transcript>, ExtractError> {
    let Some(transcript) = library
        .transcript(source)
        .map_err(|e| ExtractError::ExportFailed(format!("{e:#}")))?
    else {
        return Ok(None);
    };
    let mut sliced_segments = Vec::new();
    for segment in &transcript.segments {
        if segment.end <= start
            || segment.start >= end
            || Transcript::display_text(&segment.text).is_empty()
        {
            continue;
        }
        let mut copy = segment.clone();
        copy.start = (segment.start - start).max(0.0);
        copy.end = (segment.end - start).min(slice_duration);
        if copy.end > copy.start {
            sliced_segments.push(copy);
        }
    }
    if sliced_segments.is_empty() {
        return Ok(None);
    }
    let surviving: BTreeSet<String> = sliced_segments
        .iter()
        .map(|s| Transcript::canonical_speaker_key(&s.speaker))
        .collect();
    let speaker_aliases = transcript
        .speaker_aliases
        .into_iter()
        .filter(|(k, _)| surviving.contains(&Transcript::canonical_speaker_key(k)))
        .collect();
    let speaker_calendar_identity_ids = transcript
        .speaker_calendar_identity_ids
        .into_iter()
        .filter(|(k, _)| surviving.contains(&Transcript::canonical_speaker_key(k)))
        .collect();
    Ok(Some(Transcript {
        segments: sliced_segments,
        engine: transcript.engine,
        speaker_aliases,
        speaker_calendar_identity_ids,
        echo_report: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Segment;
    use crate::paths::LibraryPaths;

    fn finished_meeting(library: &Library, title: &str, duration: f64) -> Meeting {
        let started = crate::time::now_seconds() - Duration::seconds(120);
        let mut meeting = library.create_meeting(title, "Zoom", started).unwrap();
        meeting.ended_at = Some(started + Duration::seconds(duration as i64));
        meeting.recorded_duration = Some(duration);
        meeting.calendar_event_id = Some("cal-1".into());
        meeting.calendar_title = Some("Original Calendar".into());
        meeting.participant_name_hints = Some(vec!["Maya".into()]);
        library.save_meeting(&meeting).unwrap();
        meeting
    }

    fn segment(start: f64, end: f64, speaker: &str, text: &str) -> Segment {
        serde_json::from_value(serde_json::json!({
            "start": start, "end": end, "speaker": speaker, "text": text
        }))
        .unwrap()
    }

    #[test]
    fn extract_creates_child_meeting_slices_transcript_and_cleans_up_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let source = finished_meeting(&library, "Two Meetings", 20.0);
        fs::write(library.meeting_dir(&source).join(MIC_TRACK), b"mic").unwrap();
        fs::write(library.meeting_dir(&source).join(SYSTEM_TRACK), b"sys").unwrap();
        library.save_notes(&source, "Keep follow-up items").unwrap();
        let mut transcript = Transcript {
            engine: "whisper".into(),
            segments: vec![
                segment(0.5, 1.5, "me", "Too early"),
                segment(1.5, 4.0, "me", "Overlapping start"),
                segment(4.2, 4.8, "them", "Inside slice"),
                segment(7.0, 9.0, "them 2", "Too late"),
            ],
            ..Default::default()
        };
        transcript.speaker_aliases.insert("me".into(), "You".into());
        transcript
            .speaker_aliases
            .insert("them".into(), "Maya".into());
        transcript
            .speaker_aliases
            .insert("them 2".into(), "Dan".into());
        transcript
            .speaker_calendar_identity_ids
            .insert("them".into(), "maya-id".into());
        transcript
            .speaker_calendar_identity_ids
            .insert("them 2".into(), "dan-id".into());
        library.save_transcript(&source, &transcript).unwrap();

        let write_dummy = |slices: &[Slice], dest: &Path| -> std::io::Result<Vec<bool>> {
            fs::write(dest, b"audio")?;
            Ok(vec![true; slices.len()])
        };
        let created = extract_meeting_from_selection(
            &library,
            &source,
            ContentRange {
                start: 2.0,
                end: 5.0,
            },
            None,
            &write_dummy,
        )
        .unwrap();

        assert_eq!(created.title, "Two Meetings (0:02–0:05)");
        assert_eq!(created.extracted_from_meeting_id, Some(source.id));
        assert_eq!(created.recorded_duration, Some(3.0));
        assert!(created.has_system_track);
        assert_eq!(created.calendar_event_id, None);
        assert_eq!(created.calendar_title, None);
        assert_eq!(created.participant_name_hints, Some(vec!["Maya".into()]));
        assert_eq!(created.started_at, source.started_plus(2.0));
        assert_eq!(
            created.ended_at,
            Some(source.started_plus(2.0) + Duration::seconds(3))
        );
        assert_eq!(library.notes(&created).unwrap(), "Keep follow-up items");

        let sliced = library.transcript(&created).unwrap().unwrap();
        assert_eq!(sliced.segments.len(), 2);
        assert!((sliced.segments[0].start - 0.0).abs() < 1e-9);
        assert!((sliced.segments[0].end - 2.0).abs() < 1e-9);
        assert!((sliced.segments[1].start - 2.2).abs() < 1e-9);
        assert!((sliced.segments[1].end - 2.8).abs() < 1e-9);
        assert!(!sliced.speaker_aliases.contains_key("them 2"));
        assert!(!sliced.speaker_calendar_identity_ids.contains_key("them 2"));
        assert_eq!(sliced.speaker_aliases["them"], "Maya");
        assert_eq!(sliced.speaker_calendar_identity_ids["them"], "maya-id");

        let source_after = library.meeting(source.id).unwrap().unwrap();
        assert_eq!(source_after.title, "Two Meetings");
        assert_eq!(
            library.transcript(&source).unwrap().unwrap().segments.len(),
            4
        );

        let no_audio = |slices: &[Slice], _: &Path| Ok(vec![false; slices.len()]);
        let err = extract_meeting_from_selection(
            &library,
            &source,
            ContentRange {
                start: 1.0,
                end: 2.0,
            },
            None,
            &no_audio,
        )
        .unwrap_err();
        assert!(matches!(err, ExtractError::NoAudioTracks));
        assert_eq!(library.list_meetings().unwrap().len(), 2);
    }

    #[test]
    fn extract_rejects_live_short_and_invalid_meetings() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(LibraryPaths::new(dir.path())).unwrap();
        let live = library
            .create_meeting("Live", "Zoom", crate::time::now_seconds())
            .unwrap();
        let noop = |_: &[Slice], _: &Path| Ok(vec![true]);
        assert!(matches!(
            extract_meeting_from_selection(
                &library,
                &live,
                ContentRange {
                    start: 0.0,
                    end: 1.0
                },
                None,
                &noop,
            ),
            Err(ExtractError::RecordingStillActive)
        ));

        let done = finished_meeting(&library, "Done", 10.0);
        assert!(matches!(
            extract_meeting_from_selection(
                &library,
                &done,
                ContentRange {
                    start: 1.0,
                    end: 1.1
                },
                None,
                &noop,
            ),
            Err(ExtractError::InvalidRange)
        ));
    }
}
