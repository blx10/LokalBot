use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{AppContext as _, Context};
use lokalbot_audio::Player;
use lokalbot_core::ContentRange;

use super::MeetingsPage;
use crate::state::{AppEvent, MeetingTab};
use crate::ui::waveform::{WaveformDrag, WaveformViewport};

impl MeetingsPage {
    pub(super) fn open_player(&mut self, cx: &mut Context<Self>) {
        self.stop_playback();
        let Some(doc) = self.doc.clone() else { return };
        if !doc.has_audio || doc.meeting.is_live() {
            return;
        }
        let (mic, system, id) = (doc.mic.clone(), doc.system.clone(), doc.meeting.id);
        let range = doc.meeting.valid_content_range().map(|r| (r.start, r.end));
        self.player_loading = true;
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move { Player::open(mic.as_deref(), system.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.doc.as_ref().map(|d| d.meeting.id) != Some(id) {
                    return;
                }
                this.player_loading = false;
                match opened {
                    Ok(player) => {
                        player.set_speed(this.speed);
                        player.set_playback_range(range);
                        if let Some((position, _)) = this.playback_memory.get(&id)
                            && player.raw_duration() > 0.
                        {
                            player.seek(*position);
                        }
                        let raw = player.raw_duration().max(0.001);
                        this.playhead = (player.position() / raw).clamp(0., 1.) as f32;
                        this.player = Some(Arc::new(player));
                        this.sync_waveform_bounds();
                    }
                    Err(err) => log::warn!("could not open audio for {id}: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn active_bounds(&self, fallback_duration: f64) -> (f64, f64, f64) {
        if let Some(player) = self.player.as_ref() {
            return (
                player.window_start(),
                player.window_end(),
                player.raw_duration(),
            );
        }
        let raw = fallback_duration.max(0.0);
        let range = self
            .doc
            .as_ref()
            .and_then(|d| d.meeting.valid_content_range());
        match range {
            Some(r) if r.start < raw && raw > 0.0 => (
                r.start.max(0.0),
                r.end.min(raw).max((r.start + 0.05).min(raw)),
                raw,
            ),
            _ => (0.0, raw, raw),
        }
    }

    pub(super) fn sync_waveform_bounds(&mut self) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let id = doc.meeting.id;
        let (start, end, _) = self.active_bounds(doc.meeting.duration_seconds());
        if self.waveform_bounds_for.map(|(m, _, _)| m) != Some(id) {
            self.waveform_selection.clear();
            self.waveform_drag = None;
            self.waveform_viewport = WaveformViewport::new(start, end);
        } else if self.waveform_bounds_for != Some((id, start, end)) {
            self.waveform_selection.clear_pending();
            self.waveform_viewport.update_bounds(start, end);
        }
        self.waveform_bounds_for = Some((id, start, end));
    }

    pub(super) fn sync_player_bounds(&mut self, cx: &mut Context<Self>) {
        let range = self
            .doc
            .as_ref()
            .and_then(|d| d.meeting.valid_content_range())
            .map(|r| (r.start, r.end));
        if let Some(player) = self.player.as_ref() {
            player.set_playback_range(range);
            let raw = player.raw_duration().max(0.001);
            self.playhead = (player.position() / raw).clamp(0., 1.) as f32;
            self.playing = player.is_playing();
        }
        self.sync_waveform_bounds();
        cx.notify();
    }

    pub(super) fn stop_playback(&mut self) {
        if let Some(player) = self.player.take() {
            player.pause();
        }
        self.poll = None;
        self.playing = false;
        self.playhead = 0.;
        self.player_loading = false;
    }

    pub(super) fn pause_playback(&mut self) {
        if let Some(player) = self.player.as_ref() {
            player.pause();
        }
        self.playing = false;
        self.poll = None;
    }

    pub(super) fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        let Some(player) = self.player.clone() else {
            return;
        };
        if player.is_playing() {
            self.pause_playback();
        } else {
            self.start(player.play(), cx);
        }
        cx.notify();
    }

    pub(super) fn play_from(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let Some(player) = self.player.clone() else {
            return;
        };
        player.seek(seconds);
        self.sync_player(cx);
        if !player.is_playing() {
            self.start(player.play(), cx);
        }
    }

    pub(super) fn play_excerpt(&mut self, start: f64, end: f64, cx: &mut Context<Self>) {
        let Some(player) = self.player.clone() else {
            return;
        };
        self.start(player.play_range(start, end), cx);
    }

    pub(super) fn seek_to_time(&mut self, seconds: f64, cx: &mut Context<Self>) {
        match self.player.clone() {
            Some(player) => {
                player.seek(seconds);
                self.sync_player(cx);
            }
            None => {
                let fallback = self
                    .doc
                    .as_ref()
                    .map_or(0.0, |d| d.meeting.duration_seconds());
                let (start, end, raw) = self.active_bounds(fallback);
                let clamped = seconds.clamp(start, end);
                self.playhead = if raw > 0.0 {
                    (clamped / raw).clamp(0., 1.) as f32
                } else {
                    0.
                };
            }
        }
        cx.notify();
    }

    pub(super) fn seek_by(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let Some(player) = self.player.clone() else {
            return;
        };
        player
            .seek((player.position() + seconds).clamp(player.window_start(), player.window_end()));
        self.sync_player(cx);
        cx.notify();
    }

    pub(super) fn set_speed(&mut self, speed: f32, cx: &mut Context<Self>) {
        self.speed = speed;
        if let Some(player) = self.player.as_ref() {
            player.set_speed(speed);
        }
        cx.notify();
    }

    pub(super) fn playback_times(&self, fallback_duration: f64) -> (f64, f64, f64) {
        let (start, end, raw) = self.active_bounds(fallback_duration);
        let active_end = if end > start { end } else { raw };
        let current = match self.player.as_ref() {
            Some(player) => player.position(),
            None => (f64::from(self.playhead) * raw).clamp(start, active_end),
        };
        (current, active_end, raw)
    }

    pub(super) fn on_waveform_mouse_down(
        &mut self,
        fraction: f32,
        x_px: f32,
        in_scrollbar: bool,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if in_scrollbar {
            self.waveform_drag = Some((WaveformDrag::Scrollbar, x_px));
        } else {
            self.waveform_focus.focus(window, cx);
            self.waveform_drag = Some((WaveformDrag::Scrub, fraction));
        }
        cx.notify();
    }

    pub(super) fn on_waveform_mouse_move(
        &mut self,
        fraction: f32,
        x_px: f32,
        width_px: f32,
        dragging: bool,
        cx: &mut Context<Self>,
    ) {
        if !dragging {
            return;
        }
        match self.waveform_drag {
            Some((WaveformDrag::Scrollbar, last_x)) => {
                let width = f64::from(width_px).max(1.0);
                let delta_x = f64::from(x_px - last_x);
                let delta_seconds = (delta_x / width) * self.waveform_viewport.total_span();
                self.waveform_viewport.pan_by(delta_seconds);
                self.waveform_drag = Some((WaveformDrag::Scrollbar, x_px));
                cx.notify();
            }
            Some((WaveformDrag::Scrub, _)) => {
                self.waveform_drag = Some((WaveformDrag::Scrub, fraction));
                cx.notify();
            }
            None => {}
        }
    }

    pub(super) fn on_waveform_mouse_up(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let drag = self.waveform_drag.take();
        if let Some((WaveformDrag::Scrub, _)) = drag {
            let clicked_time = self.waveform_viewport.time_at_fraction(f64::from(fraction));
            self.seek_to_time(clicked_time, cx);
            let fallback = self
                .doc
                .as_ref()
                .map_or(0.0, |d| d.meeting.duration_seconds());
            let (start, end, _) = self.active_bounds(fallback);
            self.waveform_selection
                .handle_left_click(clicked_time, (start, end));
        }
        cx.notify();
    }

    pub(super) fn trim_to_selection(&mut self, range: ContentRange, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        if self.selection_busy {
            return;
        }
        if self.data.read(cx).is_processing(doc.meeting.id) {
            self.data.update(cx, |data, cx| {
                data.show_error(
                    lokalbot_i18n::tr(
                        "Wait for meeting processing to finish before changing its boundaries.",
                    ),
                    cx,
                );
            });
            return;
        }
        self.selection_busy = true;
        cx.notify();
        let library = self.data.read(cx).library.clone();
        let meeting = doc.meeting.clone();
        let data = self.data.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    lokalbot_engine::library_ops::set_meeting_boundaries(&library, &meeting, range)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.selection_busy = false;
                match result {
                    Ok(updated) => {
                        this.waveform_selection.clear();
                        let range = updated.valid_content_range().map(|r| (r.start, r.end));
                        if let Some(player) = this.player.as_ref() {
                            player.set_playback_range(range);
                            let raw = player.raw_duration().max(0.001);
                            this.playhead = (player.position() / raw).clamp(0., 1.) as f32;
                            this.playing = player.is_playing();
                            let (start, end) = (player.window_start(), player.window_end());
                            this.waveform_viewport.update_bounds(start, end);
                            this.waveform_bounds_for = Some((updated.id, start, end));
                        }
                        data.update(cx, |data, cx| {
                            data.request(
                                AppEvent::ProcessMeeting {
                                    meeting_id: updated.id,
                                    transcribe: true,
                                    summarize: true,
                                },
                                cx,
                            );
                            data.reload(cx);
                        });
                    }
                    Err(err) => {
                        let message: String = if err.is::<lokalbot_core::library::BoundaryError>() {
                            lokalbot_i18n::tr(
                                "Choose a start and end within the recorded audio, with the end after the start.",
                            )
                            .into()
                        } else {
                            lokalbot_i18n::format(
                                "The boundaries could not be saved. {err}",
                                &[("err", format!("{err:#}"))],
                            )
                        };
                        data.update(cx, |data, cx| data.show_error(message, cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn restore_full_recording(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        let (_, _, raw) = self.active_bounds(doc.meeting.duration_seconds());
        if raw > 0.0 {
            self.trim_to_selection(
                ContentRange {
                    start: 0.0,
                    end: raw,
                },
                cx,
            );
        }
    }

    pub(super) fn extract_selection(&mut self, range: ContentRange, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.clone() else { return };
        if self.selection_busy {
            return;
        }
        if self.data.read(cx).is_processing(doc.meeting.id) {
            self.data.update(cx, |data, cx| {
                data.show_error(
                    lokalbot_i18n::tr(
                        "Wait for meeting processing to finish before creating a meeting from a selection.",
                    ),
                    cx,
                );
            });
            return;
        }
        self.selection_busy = true;
        cx.notify();
        let library = self.data.read(cx).library.clone();
        let meeting = doc.meeting.clone();
        let data = self.data.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    lokalbot_engine::library_ops::extract_meeting_from_selection(
                        &library, &meeting, range,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.selection_busy = false;
                match result {
                    Ok(created) => {
                        this.waveform_selection.clear();
                        data.update(cx, |data, cx| {
                            if !data.meetings.iter().any(|m| m.id == created.id) {
                                data.meetings.push(created.clone());
                                data.meetings
                                    .sort_by_key(|m| std::cmp::Reverse(m.started_at));
                                data.revision += 1;
                            }
                            data.reload(cx);
                            data.open_meeting(created.id, MeetingTab::Summary, None, cx);
                            data.request(
                                AppEvent::ProcessMeeting {
                                    meeting_id: created.id,
                                    transcribe: true,
                                    summarize: true,
                                },
                                cx,
                            );
                        });
                    }
                    Err(err) => {
                        data.update(cx, |data, cx| data.show_error(err.localized(), cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        if let Err(err) = result {
            let message = format!("{err:#}");
            self.data
                .update(cx, |data, cx| data.show_error(message, cx));
            return;
        }
        self.playing = true;
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let playing = this
                    .update(cx, |this, cx| this.sync_player(cx))
                    .unwrap_or(false);
                if !playing {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn sync_player(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(player) = self.player.clone() else {
            return false;
        };
        let position = player.position();
        let duration = player.raw_duration().max(0.001);
        self.playhead = (position / duration).clamp(0., 1.) as f32;
        self.playing = player.is_playing();
        if self.playing {
            self.focus_at = Some(position);
        } else {
            self.poll = None;
        }
        cx.notify();
        self.playing
    }
}
