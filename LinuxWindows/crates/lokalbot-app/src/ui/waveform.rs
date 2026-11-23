use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Bounds, Corners, ElementId, FocusHandle, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, KeyDownEvent, MouseButton, ParentElement as _, PathBuilder, PinchEvent, Pixels,
    Point, Rgba, ScrollDelta, ScrollWheelEvent, StatefulInteractiveElement as _, Styled as _,
    Window, canvas, div, point, px, quad, size, transparent_black,
};
use lokalbot_core::ContentRange;

use crate::theme::{LbThemeExt as _, ty};

pub const HEIGHT: Pixels = px(60.);
pub const COMPACT_HEIGHT: Pixels = px(36.);
pub const MIN_ZOOM: f64 = 1.0;
pub const MAX_ZOOM: f64 = 30.0;
pub const MINIMUM_SELECTION_DURATION: f64 = 0.25;

pub fn selection_yellow() -> Hsla {
    Rgba {
        r: 0.96,
        g: 0.76,
        b: 0.14,
        a: 1.0,
    }
    .into()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformViewport {
    window_start: f64,
    window_end: f64,
    zoom: f64,
    visible_start: f64,
}

impl Default for WaveformViewport {
    fn default() -> Self {
        Self::new(0.0, 0.0)
    }
}

impl WaveformViewport {
    pub fn new(window_start: f64, window_end: f64) -> Self {
        let safe_start = if window_start.is_finite() {
            window_start.max(0.0)
        } else {
            0.0
        };
        let safe_end = if window_end.is_finite() {
            window_end.max(safe_start)
        } else {
            safe_start
        };
        Self {
            window_start: safe_start,
            window_end: safe_end,
            zoom: MIN_ZOOM,
            visible_start: safe_start,
        }
    }

    pub fn clamp_zoom(value: f64) -> f64 {
        if value.is_finite() {
            value.clamp(MIN_ZOOM, MAX_ZOOM)
        } else {
            MIN_ZOOM
        }
    }

    pub fn window_start(&self) -> f64 {
        self.window_start
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    pub fn visible_start(&self) -> f64 {
        self.visible_start
    }

    pub fn total_span(&self) -> f64 {
        (self.window_end - self.window_start).max(0.0)
    }

    pub fn visible_span(&self) -> f64 {
        let total = self.total_span();
        if total > 0.0 { total / self.zoom } else { 0.0 }
    }

    pub fn visible_end(&self) -> f64 {
        (self.visible_start + self.visible_span()).min(self.window_end)
    }

    pub fn update_bounds(&mut self, window_start: f64, window_end: f64) {
        let safe_start = if window_start.is_finite() {
            window_start.max(0.0)
        } else {
            0.0
        };
        let safe_end = if window_end.is_finite() {
            window_end.max(safe_start)
        } else {
            safe_start
        };
        if safe_start == self.window_start && safe_end == self.window_end {
            return;
        }
        self.window_start = safe_start;
        self.window_end = safe_end;
        self.clamp();
    }

    pub fn clamp(&mut self) {
        self.zoom = Self::clamp_zoom(self.zoom);
        let span = self.visible_span();
        let max_start = (self.window_end - span).max(self.window_start);
        if !self.visible_start.is_finite() {
            self.visible_start = self.window_start;
        }
        self.visible_start = self.visible_start.clamp(self.window_start, max_start);
    }

    pub fn time_at_fraction(&self, fraction: f64) -> f64 {
        let clamped = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.visible_start + clamped * self.visible_span()
    }

    pub fn fraction_at(&self, time: f64) -> f64 {
        let span = self.visible_span();
        if span > 0.0 && time.is_finite() {
            (time - self.visible_start) / span
        } else {
            0.0
        }
    }

    pub fn zoom_by(&mut self, factor: f64, anchor_fraction: f64) {
        if self.total_span() <= 0.0 || !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let clamped_anchor = if anchor_fraction.is_finite() {
            anchor_fraction.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let anchor_time = self.time_at_fraction(clamped_anchor);
        let new_zoom = Self::clamp_zoom(self.zoom * factor);
        if (new_zoom - self.zoom).abs() < f64::EPSILON {
            return;
        }
        self.zoom = new_zoom;
        self.visible_start = anchor_time - clamped_anchor * self.visible_span();
        self.clamp();
    }

    pub fn pan_by(&mut self, delta_seconds: f64) {
        if self.total_span() <= 0.0 || !delta_seconds.is_finite() {
            return;
        }
        self.visible_start += delta_seconds;
        self.clamp();
    }

    pub fn reset(&mut self) {
        self.zoom = MIN_ZOOM;
        self.visible_start = self.window_start;
        self.clamp();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaveformSelectionState {
    pending_start: Option<f64>,
    selection: Option<ContentRange>,
}

impl WaveformSelectionState {
    pub fn handle_left_click(&mut self, raw_time: f64, bounds: (f64, f64)) -> Option<ContentRange> {
        if !raw_time.is_finite() {
            return self.selection;
        }
        let (lo_bound, hi_bound) = (bounds.0.min(bounds.1), bounds.0.max(bounds.1));
        let time = raw_time.clamp(lo_bound, hi_bound);
        if self.selection.is_some() {
            self.selection = None;
            self.pending_start = Some(time);
            return None;
        }
        let Some(anchor) = self.pending_start else {
            self.pending_start = Some(time);
            return None;
        };
        let lower = anchor.min(time);
        let upper = anchor.max(time);
        if upper - lower >= MINIMUM_SELECTION_DURATION {
            self.selection = Some(ContentRange {
                start: lower,
                end: upper,
            });
            self.pending_start = None;
        } else {
            self.pending_start = Some(time);
        }
        self.selection
    }

    pub fn clear(&mut self) {
        self.pending_start = None;
        self.selection = None;
    }

    pub fn selection(&self) -> Option<ContentRange> {
        self.selection
    }

    pub fn pending_start(&self) -> Option<f64> {
        self.pending_start
    }

    pub fn has_any(&self) -> bool {
        self.selection.is_some() || self.pending_start.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaveformDrag {
    Scrub,
    Scrollbar,
}

#[derive(Clone, Copy)]
struct Colors {
    bars: Hsla,
    played: Hsla,
    playhead: Hsla,
    hover: Hsla,
    selection: Hsla,
    scrollbar_track: Hsla,
    scrollbar_thumb: Hsla,
}

pub type HoverHandler = Box<dyn Fn(Option<f32>, &mut Window, &mut App)>;
pub type MouseDownHandler = Box<dyn Fn(f32, f32, bool, &mut Window, &mut App)>;
pub type MouseMoveHandler = Box<dyn Fn(f32, f32, f32, bool, &mut Window, &mut App)>;
pub type MouseUpHandler = Box<dyn Fn(f32, &mut Window, &mut App)>;
pub type ZoomHandler = Box<dyn Fn(f64, f64, &mut Window, &mut App)>;
pub type PanHandler = Box<dyn Fn(f64, &mut Window, &mut App)>;
pub type ActionHandler = Box<dyn Fn(&mut Window, &mut App)>;
pub type RangeActionHandler = Box<dyn Fn(ContentRange, &mut Window, &mut App)>;

pub struct WaveformProps {
    pub peaks: Option<Arc<[f32]>>,
    pub current_time: f64,
    pub duration: f64,
    pub hover: Option<f32>,
    pub scrub_progress: Option<f32>,
    pub viewport: WaveformViewport,
    pub selection: WaveformSelectionState,
    pub height: Pixels,
    pub is_busy: bool,
    pub is_dragging: bool,
    pub allow_escape_clear: bool,
    pub has_trimmed_boundaries: bool,
    pub focus_handle: FocusHandle,
}

pub struct WaveformHandlers {
    pub on_hover: HoverHandler,
    pub on_mouse_down: MouseDownHandler,
    pub on_mouse_move: MouseMoveHandler,
    pub on_mouse_up: MouseUpHandler,
    pub on_zoom: ZoomHandler,
    pub on_pan: PanHandler,
    pub on_reset_zoom: ActionHandler,
    pub on_clear_selection: ActionHandler,
    pub on_trim_selection: RangeActionHandler,
    pub on_extract_selection: RangeActionHandler,
    pub on_restore_full: ActionHandler,
}

fn fraction(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
    let width = f32::from(bounds.size.width).max(1.);
    (f32::from(position.x - bounds.origin.x) / width).clamp(0., 1.)
}

pub fn waveform<T: Into<ElementId>>(
    id: T,
    props: WaveformProps,
    handlers: WaveformHandlers,
    cx: &App,
) -> impl IntoElement + use<T> {
    let lb = cx.lb();
    let colors = Colors {
        bars: lb.text_secondary.opacity(0.5),
        played: lb.accent_text,
        playhead: lb.text,
        hover: lb.text_secondary,
        selection: selection_yellow(),
        scrollbar_track: lb.text_secondary.opacity(0.18),
        scrollbar_thumb: lb.text_secondary.opacity(0.55),
    };
    let bounds_cell = Rc::new(Cell::new(Bounds::default()));
    let viewport = props.viewport;
    let selection = props.selection;
    let duration = props.duration;
    let current_time = props.current_time;
    let hover = props.hover;
    let scrub_progress = props.scrub_progress;
    let preview = scrub_progress.or(hover);

    let paint_bounds = bounds_cell.clone();
    let painter = canvas(
        move |bounds, _, _| paint_bounds.set(bounds),
        move |bounds, _, window, _| {
            paint_waveform(
                bounds,
                props.peaks.as_deref(),
                duration,
                current_time,
                hover,
                scrub_progress,
                viewport,
                selection,
                colors,
                window,
            )
        },
    )
    .absolute()
    .size_full();

    let on_hover = Rc::new(handlers.on_hover);
    let move_hover = on_hover.clone();
    let move_bounds = bounds_cell.clone();
    let down_bounds = bounds_cell.clone();
    let up_bounds = bounds_cell.clone();
    let up_out_bounds = bounds_cell.clone();
    let wheel_bounds = bounds_cell.clone();
    let pinch_bounds = bounds_cell;

    let on_mouse_down = handlers.on_mouse_down;
    let on_mouse_move = handlers.on_mouse_move;
    let on_mouse_up = Rc::new(handlers.on_mouse_up);
    let up_out_cb = on_mouse_up.clone();
    let on_zoom = Rc::new(handlers.on_zoom);
    let pinch_zoom = on_zoom.clone();
    let on_pan = handlers.on_pan;
    let on_reset_zoom = handlers.on_reset_zoom;
    let on_clear_selection = Rc::new(handlers.on_clear_selection);
    let key_clear = on_clear_selection.clone();
    let on_trim_selection = Rc::new(handlers.on_trim_selection);
    let on_extract_selection = Rc::new(handlers.on_extract_selection);
    let on_restore_full = Rc::new(handlers.on_restore_full);

    let is_dragging = props.is_dragging;
    let allow_escape_clear = props.allow_escape_clear;
    let zoom_badge = (viewport.zoom() > 1.01).then(|| {
        let label = format!("{}x", viewport.zoom().round() as u32);
        div()
            .id("waveform-reset-zoom")
            .absolute()
            .top(px(4.))
            .right(px(4.))
            .px(px(5.))
            .py(px(1.))
            .rounded(px(4.))
            .bg(lb.elevated.opacity(0.9))
            .border_1()
            .border_color(lb.separator)
            .cursor_pointer()
            .text_size(ty::SMALL)
            .font_weight(FontWeight::MEDIUM)
            .text_color(lb.text)
            .child(label)
            .tooltip(|window, cx| Tooltip::new(lokalbot_i18n::tr("Reset zoom")).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                if !is_dragging {
                    cx.stop_propagation();
                }
            })
            .on_click(move |_, window, cx| {
                if !is_dragging {
                    cx.stop_propagation();
                    on_reset_zoom(window, cx);
                }
            })
    });

    let current_selection = selection.selection();
    let has_selection_or_pending = selection.has_any();
    let is_busy = props.is_busy;
    let has_trimmed = props.has_trimmed_boundaries;

    div()
        .id(id)
        .relative()
        .flex_1()
        .min_w(px(80.))
        .h(props.height)
        .track_focus(&props.focus_handle)
        .child(painter)
        .when_some(preview, |this, preview| {
            let stamp = lokalbot_core::time::stamp(viewport.time_at_fraction(f64::from(preview)));
            this.child(
                div()
                    .absolute()
                    .top(px(-4.))
                    .left(gpui_kit::relative(preview))
                    .ml(px(-28.))
                    .w(px(56.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(px(4.))
                            .rounded(px(3.))
                            .bg(lb.elevated)
                            .border_1()
                            .border_color(lb.separator)
                            .text_size(ty::SMALL)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(lb.text)
                            .child(stamp),
                    ),
            )
        })
        .children(zoom_badge)
        .on_mouse_move(move |event, window, cx| {
            let bounds = move_bounds.get();
            let inside = bounds.contains(&event.position);
            let at = fraction(bounds, event.position);
            if inside {
                move_hover(Some(at), window, cx);
            }
            let width = f32::from(bounds.size.width).max(1.);
            on_mouse_move(
                at,
                f32::from(event.position.x),
                width,
                event.pressed_button == Some(MouseButton::Left),
                window,
                cx,
            );
        })
        .on_hover(move |hovered, window, cx| {
            if !*hovered {
                on_hover(None, window, cx);
            }
        })
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            let bounds = down_bounds.get();
            let local_y = f32::from(event.position.y - bounds.origin.y);
            let height_f = f32::from(bounds.size.height);
            let in_scrollbar = viewport.zoom() > 1.01 && local_y >= height_f - 8.;
            on_mouse_down(
                fraction(bounds, event.position),
                f32::from(event.position.x),
                in_scrollbar,
                window,
                cx,
            );
        })
        .on_mouse_up(MouseButton::Left, move |event, window, cx| {
            let bounds = up_bounds.get();
            on_mouse_up(fraction(bounds, event.position), window, cx);
        })
        .on_mouse_up_out(MouseButton::Left, move |event, window, cx| {
            let bounds = up_out_bounds.get();
            up_out_cb(fraction(bounds, event.position), window, cx);
        })
        .on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
            let bounds = wheel_bounds.get();
            let anchor = f64::from(fraction(bounds, event.position));
            let (dx, dy, precise) = match event.delta {
                ScrollDelta::Pixels(p) => (f32::from(p.x) as f64, f32::from(p.y) as f64, true),
                ScrollDelta::Lines(p) => (p.x as f64, p.y as f64, false),
            };
            let is_horizontal = event.modifiers.shift || dx.abs() > dy.abs();
            if is_horizontal {
                if viewport.zoom() > 1.001 {
                    let raw_delta = if dx.abs() > dy.abs() { dx } else { dy };
                    let pixel_delta = if precise { raw_delta } else { raw_delta * 24.0 };
                    let width = f32::from(bounds.size.width).max(1.) as f64;
                    let delta_seconds = (-pixel_delta / width) * viewport.visible_span();
                    on_pan(delta_seconds, window, cx);
                    cx.stop_propagation();
                }
                return;
            }
            if dy.abs() > 0.001 {
                let step = if precise { dy * 0.01 } else { dy * 0.12 };
                let factor = step.clamp(-0.6, 0.6).exp();
                on_zoom(factor, anchor, window, cx);
                cx.stop_propagation();
            }
        })
        .on_pinch(move |event: &PinchEvent, window, cx| {
            let bounds = pinch_bounds.get();
            let anchor = f64::from(fraction(bounds, event.position));
            let factor = (1.0 + f64::from(event.delta)).clamp(0.5, 2.0);
            pinch_zoom(factor, anchor, window, cx);
            cx.stop_propagation();
        })
        .on_key_down(move |event: &KeyDownEvent, window, cx| {
            if event.keystroke.key.as_str() == "escape"
                && allow_escape_clear
                && has_selection_or_pending
            {
                key_clear(window, cx);
                cx.stop_propagation();
            }
        })
        .tooltip(|window, cx| {
            Tooltip::new(lokalbot_i18n::tr(
                "Click to seek and mark selection start/end. Scroll wheel zooms, Esc clears selection.",
            ))
            .build(window, cx)
        })
        .context_menu(move |menu, _, _| {
            let trim_cb = on_trim_selection.clone();
            let extract_cb = on_extract_selection.clone();
            let clear_cb = on_clear_selection.clone();
            let restore_cb = on_restore_full.clone();
            let mut menu = menu
                .item(
                    PopupMenuItem::new(lokalbot_i18n::tr("Trim to Selection"))
                        .disabled(current_selection.is_none() || is_busy)
                        .on_click(move |_, window, cx| {
                            if let Some(range) = current_selection {
                                trim_cb(range, window, cx);
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new(lokalbot_i18n::tr("New Meeting from Selection"))
                        .disabled(current_selection.is_none() || is_busy)
                        .on_click(move |_, window, cx| {
                            if let Some(range) = current_selection {
                                extract_cb(range, window, cx);
                            }
                        }),
                );
            if has_selection_or_pending {
                menu = menu.separator().item(
                    PopupMenuItem::new(lokalbot_i18n::tr("Clear Selection")).on_click(
                        move |_, window, cx| {
                            clear_cb(window, cx);
                        },
                    ),
                );
            }
            if has_trimmed {
                menu = menu.separator().item(
                    PopupMenuItem::new(lokalbot_i18n::tr("Use full recording"))
                        .disabled(is_busy)
                        .on_click(move |_, window, cx| {
                            restore_cb(window, cx);
                        }),
                );
            }
            menu
        })
}

#[allow(clippy::too_many_arguments)]
fn paint_waveform(
    bounds: Bounds<Pixels>,
    peaks: Option<&[f32]>,
    duration: f64,
    current_time: f64,
    hover: Option<f32>,
    scrub_progress: Option<f32>,
    viewport: WaveformViewport,
    selection: WaveformSelectionState,
    colors: Colors,
    window: &mut Window,
) {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    if width <= 0. || height <= 0. {
        return;
    }
    let zoomed = viewport.zoom() > 1.01;
    let bottom_reserve = if zoomed { 5. } else { 0. };
    let bar_area_height = (height - bottom_reserve).max(8.);
    let inset = if bar_area_height < 32. { 4. } else { 10. };
    let center = bar_area_height / 2.;
    let origin = bounds.origin;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        Bounds::new(origin + point(px(x), px(y)), size(px(w), px(h)))
    };
    let fill = |window: &mut Window, b: Bounds<Pixels>, radius: f32, color: Hsla| {
        window.paint_quad(quad(
            b,
            Corners::all(px(radius)),
            color,
            px(0.),
            transparent_black(),
            Default::default(),
        ));
    };

    let playhead_fraction =
        scrub_progress.unwrap_or_else(|| viewport.fraction_at(current_time) as f32);
    let cut = width * playhead_fraction.clamp(0., 1.);

    let sel_x_range = selection.selection().map(|range| {
        let sx = (viewport.fraction_at(range.start) as f32) * width;
        let ex = (viewport.fraction_at(range.end) as f32) * width;
        (sx, ex)
    });

    if let Some((sx, ex)) = sel_x_range {
        let left = sx.min(ex).clamp(0., width);
        let right = sx.max(ex).clamp(0., width);
        if right > left {
            fill(
                window,
                rect(left, 0., right - left, bar_area_height),
                0.,
                colors.selection.opacity(0.18),
            );
        }
    } else if let (Some(pending), Some(hover_frac)) = (selection.pending_start(), hover) {
        let pending_x = (viewport.fraction_at(pending) as f32) * width;
        let hover_x = hover_frac * width;
        let left = pending_x.min(hover_x).clamp(0., width);
        let right = pending_x.max(hover_x).clamp(0., width);
        if right > left {
            fill(
                window,
                rect(left, 0., right - left, bar_area_height),
                0.,
                colors.selection.opacity(0.10),
            );
        }
    }

    let bars = peaks
        .map(|peaks| {
            lokalbot_audio::resample_peaks(
                peaks,
                duration,
                viewport.visible_start(),
                viewport.visible_end(),
                f64::from(width),
            )
        })
        .unwrap_or_default();

    let sel_band = sel_x_range.map(|(sx, ex)| (sx.min(ex), sx.max(ex)));

    if bars.is_empty() {
        fill(window, rect(0., center - 1., width, 2.), 1., colors.bars);
        if cut > 0. {
            fill(window, rect(0., center - 1., cut, 2.), 1., colors.played);
        }
    } else {
        let step = width / bars.len() as f32;
        let bar_width = (step - 2.).max(1.);
        for (ix, peak) in bars.iter().enumerate() {
            let x = ix as f32 * step;
            let h = (peak * (bar_area_height - inset)).max(2.);
            let in_selection = sel_band
                .is_some_and(|(left, right)| x + bar_width > left && x < right && right > left);
            let color = if in_selection {
                colors.selection
            } else if x + bar_width * 0.5 <= cut {
                colors.played
            } else {
                colors.bars
            };
            fill(
                window,
                rect(x, center - h / 2., bar_width, h),
                bar_width / 2.,
                color,
            );
        }
    }

    if let Some((sx, ex)) = sel_x_range {
        for boundary_x in [sx, ex] {
            if boundary_x >= -2. && boundary_x <= width + 2. {
                let clamped_x = (boundary_x - 1.).clamp(0., (width - 2.).max(0.));
                fill(
                    window,
                    rect(clamped_x, 0., 2., bar_area_height),
                    1.,
                    colors.selection,
                );
            }
        }
    } else if let Some(pending) = selection.pending_start() {
        let pending_x = (viewport.fraction_at(pending) as f32) * width;
        if pending_x >= -2. && pending_x <= width + 2. {
            let clamped_x = (pending_x - 1.).clamp(0., (width - 2.).max(0.));
            fill(
                window,
                rect(clamped_x, 0., 2., bar_area_height),
                1.,
                colors.selection,
            );
        }
    }

    if scrub_progress.is_some() || (0.0..=1.0).contains(&playhead_fraction) {
        let playhead_x = (cut - 1.).clamp(0., (width - 2.).max(0.));
        fill(
            window,
            rect(
                playhead_x,
                inset * 0.3,
                2.,
                (bar_area_height - inset * 0.6).max(2.),
            ),
            1.,
            colors.playhead,
        );
    }

    if let Some(hover) = hover
        && scrub_progress.is_none()
    {
        let x = (width * hover).clamp(0., (width - 1.).max(0.));
        fill(
            window,
            rect(x, inset, 1., (bar_area_height - inset * 1.5).max(2.)),
            0.,
            colors.hover,
        );
    }

    if zoomed && viewport.total_span() > 0.0 {
        fill(
            window,
            rect(0., height - 3., width, 3.),
            1.5,
            colors.scrollbar_track,
        );
        let thumb_x = (((viewport.visible_start() - viewport.window_start())
            / viewport.total_span()) as f32)
            * width;
        let thumb_w = (((viewport.visible_span() / viewport.total_span()) as f32) * width)
            .max(18.)
            .min(width);
        let clamped_thumb_x = thumb_x.clamp(0., (width - thumb_w).max(0.));
        fill(
            window,
            rect(clamped_thumb_x, height - 3., thumb_w, 3.),
            1.5,
            colors.scrollbar_thumb,
        );
    }
}

pub fn play_glyph(glyph: Pixels, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let at = |x: f32, y: f32| bounds.origin + point(px(w * x), px(h * y));
            let mut path = PathBuilder::fill();
            path.move_to(at(0.24, 0.14));
            path.line_to(at(0.90, 0.5));
            path.line_to(at(0.24, 0.86));
            path.close();
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size(glyph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_zooms_around_anchor_pans_and_resets() {
        let mut viewport = WaveformViewport::new(10.0, 110.0);
        assert_eq!(viewport.total_span(), 100.0);
        assert_eq!(viewport.visible_start(), 10.0);
        assert_eq!(viewport.visible_end(), 110.0);

        viewport.zoom_by(2.0, 0.5);
        assert!((viewport.zoom() - 2.0).abs() < 1e-9);
        assert!((viewport.visible_span() - 50.0).abs() < 1e-9);
        assert!((viewport.visible_start() - 35.0).abs() < 1e-9);
        assert!((viewport.visible_end() - 85.0).abs() < 1e-9);

        viewport.pan_by(100.0);
        assert!((viewport.visible_start() - 60.0).abs() < 1e-9);
        assert!((viewport.visible_end() - 110.0).abs() < 1e-9);

        viewport.pan_by(-100.0);
        assert!((viewport.visible_start() - 10.0).abs() < 1e-9);

        viewport.zoom_by(100.0, 0.25);
        assert!((viewport.zoom() - MAX_ZOOM).abs() < 1e-9);

        viewport.reset();
        assert!((viewport.zoom() - MIN_ZOOM).abs() < 1e-9);
        assert!((viewport.visible_start() - 10.0).abs() < 1e-9);
        assert!((viewport.visible_end() - 110.0).abs() < 1e-9);
    }

    #[test]
    fn viewport_clamps_visible_window_when_bounds_shrink() {
        let mut viewport = WaveformViewport::new(0.0, 100.0);
        viewport.zoom_by(4.0, 0.9);
        viewport.update_bounds(5.0, 30.0);
        assert!(viewport.visible_start() >= 5.0);
        assert!(viewport.visible_end() <= 30.0);
    }

    #[test]
    fn selection_state_implements_two_click_cycle_and_minimum_span() {
        let mut state = WaveformSelectionState::default();
        assert_eq!(state.handle_left_click(12.0, (0.0, 60.0)), None);
        assert_eq!(state.pending_start(), Some(12.0));

        assert_eq!(state.handle_left_click(12.1, (0.0, 60.0)), None);
        assert_eq!(state.pending_start(), Some(12.1));

        let range = state.handle_left_click(5.0, (0.0, 60.0)).unwrap();
        assert_eq!(
            range,
            ContentRange {
                start: 5.0,
                end: 12.1
            }
        );
        assert_eq!(state.pending_start(), None);

        assert_eq!(state.handle_left_click(90.0, (0.0, 60.0)), None);
        assert_eq!(state.selection(), None);
        assert_eq!(state.pending_start(), Some(60.0));

        state.clear();
        assert_eq!(state.selection(), None);
        assert_eq!(state.pending_start(), None);
    }

    #[test]
    fn selection_state_has_any_tracks_pending_and_completed_ranges() {
        let mut state = WaveformSelectionState::default();
        assert!(!state.has_any());

        state.handle_left_click(5.0, (0.0, 30.0));
        assert!(state.has_any());

        state.handle_left_click(10.0, (0.0, 30.0));
        assert!(state.has_any());

        state.clear();
        assert!(!state.has_any());
    }
}
