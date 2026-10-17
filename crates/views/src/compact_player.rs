//! A second companion window. It shares playback, but owns its own layout and saved frame.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, App, Bounds, Context, Entity, FocusHandle, FontWeight, MouseButton,
    Pixels, Render, ScrollWheelEvent, SharedString, Size, Subscription, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
    div, point, px, size,
};
use i18n::t;
use input::{CloseWindow, OpenPlayerLyrics, OpenPlayerQueue, ToggleCompactPlayer, ToggleMiniPlayer, WORKSPACE_CONTEXT};
use state::{AppSettings, Playback, Queue, Sonora};
use ui::{ActiveTheme as _, Artwork, Button, Dismiss, ExplicitBadge, Scrubber, ScrubberState, Text, clock, tabular};

#[cfg(target_os = "windows")]
use crate::mini_player::native;
use crate::shared::transport::{NOTCH, next, previous, toggle, volume_icon};

const WIDTH_COVERS: f32 = 2.5;
const HEIGHT_CONTROLS: f32 = 5.;
const ARTWORK_CONTROLS: f32 = 1.75;
const SCALES: [f32; 3] = [0.9, 1., 1.15];
const PIXEL: Pixels = px(1.);

/// A compact transport card independent of the artwork-only mini player.
pub(crate) struct CompactPlayer {
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    settings: Entity<AppSettings>,
    parent: AnyWindowHandle,
    focus: FocusHandle,
    seek: ScrubberState,
    pending: Option<f32>,
    muted: Option<f32>,
    menu_open: bool,
    _bounds_watch: Subscription,
}

impl CompactPlayer {
    /// Opens a separate card using the existing engine, retaining its own last position and size.
    pub(crate) fn open(parent: &mut Window, cx: &mut App) -> Option<WindowHandle<Self>> {
        let settings = Sonora::global(cx).settings.clone();
        let saved = settings.read(cx).compact_player_bounds();
        let display = saved.and_then(|bounds| cx.displays().into_iter()
            .find(|display| display.bounds().contains(&bounds.center())))
            .or_else(|| parent.display(cx));
        let display_id = display.as_ref().map(|display| display.id());
        let mut bounds = saved.unwrap_or_else(|| Bounds::centered(display_id, dimensions(1., cx), cx));
        if let Some(display) = display {
            bounds = fitted(bounds, display.visible_bounds(), Pixels::ZERO, cx);
        }
        let on_top = settings.read(cx).compact_player_on_top();
        let parent = parent.window_handle();
        cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id,
            titlebar: None,
            kind: WindowKind::PopUp,
            window_background: WindowBackgroundAppearance::Blurred,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            app_owns_titlebar_drag: true,
            app_id: Some("sonora".into()),
            ..Default::default()
        }, move |window, cx| {
            apply(window, Some(bounds), Some(on_top), cx);
            window.on_window_should_close(cx, move |_, cx| {
                restore(parent, cx);
                true
            });
            cx.new(|cx| {
                let playback = Sonora::global(cx).playback.clone();
                let queue = Sonora::global(cx).queue.clone();
                cx.observe(&playback, |_, _, cx| cx.notify()).detach();
                cx.observe(&queue, |_, _, cx| cx.notify()).detach();
                cx.observe(&settings, |_, _, cx| cx.notify()).detach();
                let focus = cx.focus_handle();
                window.focus(&focus, cx);
                settings.update(cx, |settings, cx| settings.keep_compact_player_frame(window, cx));
                let bounds_watch = cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                    this.settings.update(cx, |settings, cx| settings.keep_compact_player_frame(window, cx));
                });
                Self {
                    playback, queue, settings, parent, focus,
                    seek: ScrubberState::new("compact-seek"),
                    pending: None, muted: None, menu_open: false,
                    _bounds_watch: bounds_watch,
                }
            })
        }).map_err(|error| log::error!("compact player: cannot open window: {error}")).ok()
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |settings, cx| settings.keep_compact_player_frame(window, cx));
        restore(self.parent, cx);
        window.remove_window();
    }

    fn turn_volume(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open { return; }
        let delta = event.delta.pixel_delta(window.line_height()).y;
        if delta == Pixels::ZERO { return; }
        cx.stop_propagation();
        let step = match delta > Pixels::ZERO { true => NOTCH, false => -NOTCH };
        let level = (self.playback.read(cx).volume() + step).clamp(0., 1.);
        self.muted = None;
        self.playback.update(cx, |playback, cx| playback.set_volume(level, cx));
    }

    fn mute(&mut self, cx: &mut Context<Self>) {
        let level = self.playback.read(cx).volume();
        let wanted = match level > 0. {
            true => { self.muted = Some(level); 0. },
            false => self.muted.take().unwrap_or(NOTCH * 14.),
        };
        self.playback.update(cx, |playback, cx| playback.set_volume(wanted, cx));
    }

    fn commit_seek(&mut self, cx: &mut Context<Self>) {
        let Some(fraction) = self.pending.take() else { return; };
        self.playback.update(cx, |playback, cx| playback.seek_fraction(fraction, cx));
        cx.notify();
    }

    /// Lyrics and queue use Sonora's existing panels; opening them leaves both companions intact.
    fn show_panel(&self, lyrics: bool, cx: &mut App) {
        self.parent.update(cx, |_, window, cx| {
            window.activate_window();
            let action: Box<dyn gpui::Action> = match lyrics {
                true => Box::new(OpenPlayerLyrics),
                false => Box::new(OpenPlayerQueue),
            };
            window.dispatch_action(action, cx);
        }).ok();
    }

    fn artwork_player(&self, cx: &mut App) {
        self.parent.update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(ToggleMiniPlayer), cx);
        }).ok();
    }

    /// Open the native menu outside a window update so its message loop can safely dispatch paints.
    #[cfg(target_os = "windows")]
    fn open_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open { return; }
        let Some(owner) = native::handle(window) else { return; };
        let labels = native::Labels {
            position: t!("mini-position").to_string(),
            size: t!("compact-size").to_string(),
            on_top: t!("mini-on-top").to_string(),
            restore: t!("player-mini-restore").to_string(),
            alternative: None,
            positions: ["mini-position-top-left", "mini-position-top-middle", "mini-position-top-right",
                "mini-position-bottom-left", "mini-position-bottom-middle", "mini-position-bottom-right"]
                .into_iter().enumerate().map(|(index, key)| ((index + 1) as u32, i18n::lookup(key, None).to_string(), false)).collect(),
            sizes: ["mini-size-small", "mini-size-medium", "mini-size-large"].into_iter().enumerate()
                .map(|(index, key)| ((index + 11) as u32, i18n::lookup(key, None).to_string(),
                    (window.viewport_size().width - dimensions(SCALES[index], cx).width).abs() < PIXEL)).collect(),
        };
        let on_top = self.settings.read(cx).compact_player_on_top();
        self.menu_open = true;
        cx.spawn_in(window, async move |this, cx| {
            let command = native::popup(owner, labels, on_top);
            this.update_in(cx, |this, window, cx| {
                this.menu_open = false;
                this.command(command, window, cx);
                cx.notify();
            }).ok();
        }).detach();
    }

    #[cfg(not(target_os = "windows"))]
    fn open_menu(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    #[cfg(target_os = "windows")]
    fn command(&mut self, command: u32, window: &mut Window, cx: &mut Context<Self>) {
        if command == 21 {
            let enabled = !self.settings.read(cx).compact_player_on_top();
            self.settings.update(cx, |settings, cx| settings.set_compact_player_on_top(enabled, cx));
            apply(window, None, Some(enabled), cx);
            return;
        }
        if command == 22 { self.close(window, cx); return; }
        let Some(display) = window.display(cx) else { return; };
        let work = display.visible_bounds();
        let current = window.bounds();
        let gap = cx.theme().metrics.inset;
        let bounds = match command {
            1..=6 => {
                let column = (command - 1) % 3;
                let x = match column {
                    0 => work.left() + gap,
                    1 => work.center().x - current.size.width / 2.,
                    _ => work.right() - gap - current.size.width,
                };
                let y = match command <= 3 {
                    true => work.top() + gap,
                    false => work.bottom() - gap - current.size.height,
                };
                Bounds { origin: point(x, y), size: current.size }
            }
            11..=13 => {
                let wanted = dimensions(SCALES[(command - 11) as usize], cx);
                Bounds { origin: current.center() - point(wanted.width / 2., wanted.height / 2.), size: wanted }
            }
            _ => return,
        };
        apply(window, Some(fitted(bounds, work, gap, cx)), None, cx);
    }
}

impl Render for CompactPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        window.set_rem_size(theme.font_size);
        let scale = window.viewport_size().width / dimensions(1., cx).width;
        let pad = theme.metrics.pad * 2. * scale;
        let artwork = theme.metrics.control * ARTWORK_CONTROLS * scale;
        let playback = self.playback.read(cx);
        let track = playback.track();
        let cover = track.and_then(|track| track.cover.clone());
        let title = track.map(|track| SharedString::from(track.name.clone()))
            .unwrap_or_else(|| t!("player-nothing-playing"));
        let detail = track.map(|track| match track.album.is_empty() {
            true => SharedString::from(track.artists.clone()),
            false => t!("compact-track-detail", artist = track.artists.clone(), album = track.album.clone()),
        }).unwrap_or_default();
        let explicit = track.is_some_and(|track| track.explicit);
        let total = track.map(|track| track.duration).unwrap_or(Duration::ZERO);
        let elapsed = self.pending.map(|fraction| total.mul_f32(fraction)).unwrap_or_else(|| playback.position());
        let progress = self.pending.unwrap_or_else(|| playback.progress());
        let seekable = track.is_some() && !total.is_zero();
        let volume = playback.volume();
        let font = crate::root::ui_font(self.settings.read(cx).font());
        let text = theme.text(Text::Small);
        let clock_width = text * match total.as_secs() >= 3600 { true => 5., false => 3.5 };

        let header = div()
            .id("compact-drag-area")
            .flex().items_center().gap(theme.metrics.pad * 2.)
            .h(artwork).flex_none().min_w_0()
            .window_control_area(gpui::WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(Artwork::new(cover).size(artwork).corner_radius(theme.radius / 2.))
            .child(div().flex().flex_col().justify_center().flex_1().min_w_0()
                .child(div().flex().items_center().min_w_0().gap_1()
                    .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(title))
                    .when(explicit, |this| this.child(ExplicitBadge::new())))
                .child(div().min_w_0().truncate().text_size(theme.text(Text::Label))
                    .text_color(theme.popover_foreground.opacity(0.65)).child(detail)));

        let timeline = div().flex().items_center().gap_1().h(theme.metrics.pad * 3.).flex_none()
            .text_size(text).font_features(tabular()).text_color(theme.popover_foreground.opacity(0.6))
            .child(div().w(clock_width).flex_none().whitespace_nowrap().child(clock(elapsed)))
            .child(div().flex_1().min_w_0().child(
                Scrubber::new(&self.seek, progress)
                    .colors(theme.popover_foreground.opacity(0.65), theme.popover_foreground.opacity(0.16), theme.popover_foreground)
                    .enabled(seekable)
                    .on_move(cx.listener(|this, fraction: &f32, _, cx| { this.pending = Some(*fraction); cx.notify(); }))
                    .on_release(cx.listener(|this, _, _, cx| this.commit_seek(cx)))
            ))
            .child(div().w(clock_width).flex_none().text_right().whitespace_nowrap()
                .child(t!("compact-remaining", time = &clock(total.saturating_sub(elapsed)))));

        let controls = div().flex().items_center().justify_between().h(theme.metrics.control).flex_none()
            .child(div().flex().items_center()
                .child(Button::new("compact-volume").ghost().backgroundless().small().icon(volume_icon(volume))
                    .tooltip_above(match volume <= 0. { true => "player-unmute", false => "player-mute" })
                    .on_click(cx.listener(|this, _, _, cx| this.mute(cx))))
                .child(Button::new("compact-options").ghost().backgroundless().small().icon("icons/ellipsis.svg")
                    .tooltip_above("compact-options").on_click(cx.listener(|this, _, window, cx| this.open_menu(window, cx)))))
            .child(div().flex().items_center().gap_1()
                .child(previous(&self.playback, false, cx).backgroundless())
                .child(toggle(&self.playback, true, false, cx).backgroundless())
                .child(next(&self.playback, &self.queue, false, cx).backgroundless()))
            .child(div().flex().items_center()
                .child(Button::new("compact-lyrics").ghost().backgroundless().small().icon("icons/mic-vocal.svg")
                    .tooltip_above("compact-lyrics").on_click(cx.listener(|this, _, _, cx| this.show_panel(true, cx))))
                .child(Button::new("compact-queue").ghost().backgroundless().small().icon("icons/list-music.svg")
                    .tooltip_above("compact-queue").on_click(cx.listener(|this, _, _, cx| this.show_panel(false, cx)))));

        div().id("compact-player").key_context(WORKSPACE_CONTEXT).track_focus(&self.focus)
            .relative().size_full().rounded(theme.radius).overflow_hidden()
            .bg(theme.popover.opacity(0.72)).border_1().border_color(theme.border.opacity(0.6))
            .text_color(theme.popover_foreground).text_size(theme.font_size).font(font)
            .flex().flex_col().justify_between().p(pad)
            .on_scroll_wheel(cx.listener(Self::turn_volume))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| {
                cx.stop_propagation(); this.open_menu(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleCompactPlayer, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleMiniPlayer, _, cx| this.artwork_player(cx)))
            .child(header).child(timeline).child(controls)
    }
}

fn dimensions(scale: f32, cx: &App) -> Size<Pixels> {
    let theme = cx.theme();
    size(theme.metrics.cover * WIDTH_COVERS * scale, theme.metrics.control * HEIGHT_CONTROLS * scale)
}

/// Preserve the card's aspect ratio and keep the whole card on the selected usable screen.
fn fitted(bounds: Bounds<Pixels>, work: Bounds<Pixels>, gap: Pixels, cx: &App) -> Bounds<Pixels> {
    let base = dimensions(1., cx);
    let gap = gap.min(work.size.width.min(work.size.height) / 4.);
    let available = ((work.size.width - gap * 2.).max(PIXEL) / base.width)
        .min((work.size.height - gap * 2.).max(PIXEL) / base.height);
    let scale = (bounds.size.width / base.width).clamp(SCALES[0].min(available), SCALES[2].min(available));
    let wanted = dimensions(scale, cx);
    Bounds {
        origin: point(bounds.origin.x.clamp(work.left() + gap, work.right() - gap - wanted.width),
            bounds.origin.y.clamp(work.top() + gap, work.bottom() - gap - wanted.height)),
        size: wanted,
    }
}

#[cfg(target_os = "windows")]
fn apply(window: &mut Window, bounds: Option<Bounds<Pixels>>, on_top: Option<bool>, cx: &mut App) {
    let Some(owner) = native::handle(window) else { return; };
    let scale = window.scale_factor();
    cx.foreground_executor().spawn(async move { native::apply(owner, bounds, scale, on_top); }).detach();
}

#[cfg(not(target_os = "windows"))]
fn apply(window: &mut Window, bounds: Option<Bounds<Pixels>>, _: Option<bool>, _: &mut App) {
    if let Some(bounds) = bounds { window.resize(bounds.size); }
}

fn restore(parent: AnyWindowHandle, cx: &mut App) {
    parent.update(cx, |_, window, _| window.activate_window()).ok();
}
