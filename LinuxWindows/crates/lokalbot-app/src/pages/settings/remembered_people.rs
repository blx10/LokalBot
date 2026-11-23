use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Task, Window, div, px,
};
use lokalbot_audio::Player;
use lokalbot_core::Meeting;
use lokalbot_core::library::MeetingAudio;
use lokalbot_core::voice::{Contribution, VoiceProfile};
use uuid::Uuid;

use crate::i18n::h_flex;
use crate::state::AppData;
use crate::theme::{LbThemeExt as _, ty};

struct Person {
    profile: VoiceProfile,
    name: Entity<InputState>,
}

struct PreviewPlayback {
    decision_id: Uuid,
    player: Arc<Player>,
}

pub(crate) struct RememberedPeople {
    data: Entity<AppData>,
    people: Vec<Person>,
    meetings: HashMap<Uuid, (Meeting, MeetingAudio)>,
    expanded: HashSet<Uuid>,
    merging_source: Option<Uuid>,
    preview: Option<PreviewPlayback>,
    preview_poll: Option<Task<()>>,
    error: Option<SharedString>,
    busy: bool,
}

fn safe(name: &str) -> bool {
    lokalbot_core::voice::safe_name(name, &|text| lokalbot_ai::sanitize::redaction(text).1 > 0)
        .is_some()
}

impl RememberedPeople {
    pub(crate) fn open(data: Entity<AppData>, window: &mut Window, cx: &mut App) {
        let sheet = cx.new(|cx| {
            let mut this = Self {
                data,
                people: Vec::new(),
                meetings: HashMap::new(),
                expanded: HashSet::new(),
                merging_source: None,
                preview: None,
                preview_poll: None,
                error: None,
                busy: false,
            };
            this.reload(window, cx);
            this
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.w(px(580.)).close_button(false).child(sheet.clone())
        });
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let library = self.data.read(cx).library.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let profiles = lokalbot_engine::voice::memory(&library).profiles()?;
                    let meetings: HashMap<Uuid, (Meeting, MeetingAudio)> = library
                        .list_meetings()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|meeting| {
                            let audio = library.audio(&meeting);
                            (meeting.id, (meeting, audio))
                        })
                        .collect();
                    Ok::<_, lokalbot_core::voice::Failure>((profiles, meetings))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((profiles, meetings)) => {
                        this.meetings = meetings;
                        this.people = profiles
                            .into_iter()
                            .map(|profile| {
                                let name = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder(crate::i18n::placeholder(
                                            "Person's name",
                                            window,
                                            cx,
                                        ))
                                        .default_value(profile.name.clone())
                                });
                                cx.subscribe_in(
                                    &name,
                                    window,
                                    |_, _, event: &InputEvent, _, cx| {
                                        if let InputEvent::Change = event {
                                            cx.notify();
                                        }
                                    },
                                )
                                .detach();
                                Person { profile, name }
                            })
                            .collect();
                        this.error = None;
                    }
                    Err(err) => {
                        this.error = Some(
                            lokalbot_i18n::format(
                                "Could not read remembered people: {err}",
                                &[("err", err.to_string())],
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn perform(
        &mut self,
        action: impl FnOnce(
            &lokalbot_core::voice::VoiceMemory,
        ) -> Result<(), lokalbot_core::voice::Failure>
        + Send
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stop_preview();
        self.merging_source = None;
        self.busy = true;
        cx.notify();
        let library = self.data.read(cx).library.clone();
        let data = self.data.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { action(&lokalbot_engine::voice::memory(&library)) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) => {
                        this.error = None;
                        this.reload(window, cx);
                        data.update(cx, |data, cx| data.mark_library_changed(cx));
                    }
                    Err(err) => {
                        this.error = Some(
                            lokalbot_i18n::format(
                                "Change pending: {err}",
                                &[("err", err.to_string())],
                            )
                            .into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rename(&mut self, id: Uuid, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.perform(move |memory| memory.rename_profile(id, &name), window, cx);
    }

    fn forget(&mut self, id: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        self.perform(move |memory| memory.forget_profile(id), window, cx);
    }

    fn merge(
        &mut self,
        source_id: Uuid,
        target_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.perform(
            move |memory| memory.merge_profiles(source_id, target_id),
            window,
            cx,
        );
    }

    fn stop_preview(&mut self) {
        if let Some(preview) = self.preview.take() {
            preview.player.pause();
        }
        self.preview_poll = None;
    }

    fn toggle_preview(
        &mut self,
        contribution: &Contribution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .preview
            .as_ref()
            .is_some_and(|p| p.decision_id == contribution.decision_id)
        {
            self.stop_preview();
            cx.notify();
            return;
        }
        self.stop_preview();
        let Some((_, audio)) = self.meetings.get(&contribution.meeting_id) else {
            return;
        };
        if audio.mic.is_none() && audio.system.is_none() {
            return;
        }
        let Some(range) = contribution.samples.first().map(|s| s.range) else {
            return;
        };
        let decision_id = contribution.decision_id;
        let mic = audio.mic.clone();
        let system = audio.system.clone();
        let start = range.start;
        let end = range.end.max(start + 6.0);
        cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_spawn(async move { Player::open(mic.as_deref(), system.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match opened {
                    Ok(player) => {
                        let player = Arc::new(player);
                        let end = if player.duration() > 0. {
                            end.min(player.duration())
                        } else {
                            end
                        };
                        if let Err(err) = player.play_range(start, end) {
                            this.error = Some(err.to_string().into());
                            cx.notify();
                            return;
                        }
                        this.preview = Some(PreviewPlayback {
                            decision_id,
                            player: player.clone(),
                        });
                        this.preview_poll = Some(cx.spawn(async move |this, cx| {
                            loop {
                                cx.background_executor()
                                    .timer(Duration::from_millis(120))
                                    .await;
                                let active = this
                                    .update(cx, |this, cx| {
                                        let still_playing =
                                            this.preview.as_ref().is_some_and(|p| {
                                                p.decision_id == decision_id
                                                    && p.player.is_playing()
                                            });
                                        if !still_playing {
                                            this.preview = None;
                                            this.preview_poll = None;
                                            cx.notify();
                                        }
                                        still_playing
                                    })
                                    .unwrap_or(false);
                                if !active {
                                    break;
                                }
                            }
                        }));
                    }
                    Err(err) => {
                        this.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn compatible_targets(&self, source: &VoiceProfile) -> Vec<VoiceProfile> {
        self.people
            .iter()
            .filter_map(|p| {
                let candidate = &p.profile;
                if candidate.id == source.id || candidate.model != source.model {
                    return None;
                }
                if let (Some(a), Some(b)) = (source.is_local_user, candidate.is_local_user)
                    && a != b
                {
                    return None;
                }
                Some(candidate.clone())
            })
            .collect()
    }

    fn render_contribution(
        &self,
        ix: usize,
        c_ix: usize,
        contribution: &Contribution,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lb = cx.lb().clone();
        let entry = self.meetings.get(&contribution.meeting_id);
        let title = entry
            .map(|(m, _)| m.display_title())
            .or_else(|| contribution.meeting_title.clone())
            .unwrap_or_else(|| lokalbot_i18n::tr("Meeting").to_string());
        let when = entry
            .map(|(m, _)| m.started_at)
            .or(contribution.meeting_started_at)
            .unwrap_or(contribution.confirmed_at);
        let when_text = format!(
            "{} {}",
            lokalbot_core::time::local_day(when),
            lokalbot_core::time::clock_time(when)
        );
        let has_audio = entry.is_some_and(|(_, a)| a.mic.is_some() || a.system.is_some());
        let can_preview = has_audio && !contribution.samples.is_empty();
        let is_playing = self
            .preview
            .as_ref()
            .is_some_and(|p| p.decision_id == contribution.decision_id);
        let snapshot = contribution.clone();
        h_flex()
            .gap(px(8.))
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(ty::CAPTION)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(lb.text)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(ty::CAPTION)
                            .text_color(lb.text_secondary)
                            .child(when_text),
                    ),
            )
            .child(if can_preview {
                Button::new(("person-preview", ix * 100 + c_ix))
                    .outline()
                    .xsmall()
                    .label(lokalbot_i18n::tr(if is_playing { "Stop" } else { "Play" }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_preview(&snapshot, window, cx);
                    }))
                    .into_any_element()
            } else {
                div()
                    .text_size(ty::CAPTION)
                    .text_color(lb.text_secondary)
                    .child(lokalbot_i18n::tr("Recording deleted"))
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_person(&self, ix: usize, person: &Person, cx: &mut Context<Self>) -> AnyElement {
        let lb = cx.lb().clone();
        let id = person.profile.id;
        let typed = person.name.read(cx).value().to_string();
        let changed = typed.trim() != person.profile.name && safe(&typed);
        let short: String = crate_id(&id).chars().take(6).collect();
        let merge_targets = self.compatible_targets(&person.profile);
        let is_merging = self.merging_source == Some(id);
        let is_expanded = self.expanded.contains(&id);
        let mut contributions = person.profile.contributions.clone();
        contributions.sort_by_key(|a| std::cmp::Reverse(a.confirmed_at));
        v_flex()
            .gap(px(6.))
            .p(px(10.))
            .rounded(px(10.))
            .bg(lb.group_fill)
            .border_1()
            .border_color(lb.group_stroke)
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&person.name).small()),
                    )
                    .child(
                        Button::new(("person-rename", ix))
                            .outline()
                            .small()
                            .label(lokalbot_i18n::tr("Rename"))
                            .disabled(!changed || self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let name = this.people[ix].name.read(cx).value().to_string();
                                this.rename(id, name, window, cx);
                            })),
                    )
                    .when(!merge_targets.is_empty(), |row| {
                        row.child(
                            Button::new(("person-merge", ix))
                                .ghost()
                                .small()
                                .label(lokalbot_i18n::tr("Merge…"))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.merging_source = if this.merging_source == Some(id) {
                                        None
                                    } else {
                                        Some(id)
                                    };
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(("person-forget", ix))
                            .danger()
                            .small()
                            .label(lokalbot_i18n::tr("Forget"))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.forget(Some(id), window, cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(ty::CAPTION)
                            .text_color(lb.text_secondary)
                            .child(lokalbot_i18n::format(
                                "{count} confirmed recording(s) · Profile {id}",
                                &[
                                    ("count", person.profile.contributions.len().to_string()),
                                    ("id", short),
                                ],
                            )),
                    )
                    .when(!contributions.is_empty(), |row| {
                        row.child(
                            Button::new(("person-sources", ix))
                                .ghost()
                                .xsmall()
                                .label(lokalbot_i18n::tr(if is_expanded {
                                    "Hide meetings"
                                } else {
                                    "Show meetings"
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.expanded.remove(&id) {
                                        this.expanded.insert(id);
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(is_merging && !merge_targets.is_empty(), |card| {
                let mut row = h_flex().flex_wrap().gap(px(6.)).items_center().child(
                    div()
                        .text_size(ty::CAPTION)
                        .text_color(lb.text_secondary)
                        .child(lokalbot_i18n::tr("Merge into:")),
                );
                for (target_ix, target) in merge_targets.iter().enumerate() {
                    let target_id = target.id;
                    row = row.child(
                        Button::new(("person-merge-target", ix * 100 + target_ix))
                            .outline()
                            .xsmall()
                            .label(target.name.clone())
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.merge(id, target_id, window, cx);
                            })),
                    );
                }
                row = row.child(
                    Button::new(("person-merge-cancel", ix))
                        .ghost()
                        .xsmall()
                        .label(lokalbot_i18n::tr("Cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.merging_source = None;
                            cx.notify();
                        })),
                );
                card.child(row)
            })
            .when(is_expanded && !contributions.is_empty(), |card| {
                let mut list = v_flex().gap(px(6.)).pt(px(4.));
                for (c_ix, contribution) in contributions.iter().enumerate() {
                    list = list.child(self.render_contribution(ix, c_ix, contribution, cx));
                }
                card.child(list)
            })
            .into_any_element()
    }
}

fn crate_id(id: &Uuid) -> String {
    lokalbot_core::ids::uuid_upper(id)
}

impl Render for RememberedPeople {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lb = cx.lb().clone();
        let rows: Vec<AnyElement> = (0..self.people.len())
            .map(|ix| {
                let person = Person {
                    profile: self.people[ix].profile.clone(),
                    name: self.people[ix].name.clone(),
                };
                self.render_person(ix, &person, cx)
            })
            .collect();
        v_flex()
            .gap(px(16.))
            .child(
                div()
                    .text_size(ty::QUESTION)
                    .font_weight(FontWeight::BOLD)
                    .text_color(lb.text)
                    .child(lokalbot_i18n::tr("Remembered people")),
            )
            .child(
                div()
                    .text_size(ty::SECONDARY)
                    .text_color(lb.text_secondary)
                    .child(crate::ui::text::paragraph(lokalbot_i18n::tr(
                        "Voice profiles stay on this computer. Forgetting a person removes their samples and identity links. Existing transcript names remain.",
                    ))),
            )
            .when(self.people.is_empty(), |this| {
                this.child(
                    div()
                        .py(px(8.))
                        .text_size(ty::BODY)
                        .text_color(lb.text_secondary)
                        .child(crate::ui::text::paragraph(lokalbot_i18n::tr(
                            "No remembered voices yet. Confirm a speaker after a recording with enough clear speech.",
                        ))),
                )
            })
            .when(!rows.is_empty(), |this| {
                this.child(
                    div()
                        .max_h(px(360.))
                        .overflow_y_scrollbar()
                        .child(v_flex().gap(px(10.)).children(rows)),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(ty::CAPTION)
                        .text_color(lb.attention_text)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("people-clear")
                            .danger()
                            .label(lokalbot_i18n::tr("Clear all"))
                            .disabled(self.people.is_empty() || self.busy)
                            .on_click(cx.listener(|this, _, window, cx| this.forget(None, window, cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("people-done")
                            .primary()
                            .label(lokalbot_i18n::tr("Done"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.stop_preview();
                                window.close_dialog(cx);
                            })),
                    ),
            )
    }
}
