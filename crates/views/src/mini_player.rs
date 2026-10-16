use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyWindowHandle, App, Bounds, Context, Entity, FocusHandle, MouseButton, Render,
    Pixels, ScrollWheelEvent, Subscription, point, px,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
    div, size,
};
use input::{CloseWindow, ToggleMiniPlayer, WORKSPACE_CONTEXT};
use state::{AppSettings, Playback, Queue, Sonora};
use ui::{ActiveTheme as _, Artwork, Button, Dismiss, Motion};

use crate::shared::transport::{NOTCH, next, previous, toggle};

#[cfg(target_os = "windows")]
mod native;

const COVER_SCALE: f32 = 2.3;
const COVER_SCALES: [f32; 3] = [1.6, COVER_SCALE, 3.1];
const PIXEL: Pixels = px(1.);

/// Explicit alignment is a one-time move; dragging always remains unrestricted.
#[derive(Clone, Copy)]
#[cfg(target_os = "windows")]
enum Position {
    TopLeft,
    TopMiddle,
    TopRight,
    BottomLeft,
    BottomMiddle,
    BottomRight,
}

#[cfg(target_os = "windows")]
impl Position {
    const ALL: [Self; 6] = [Self::TopLeft, Self::TopMiddle, Self::TopRight,
        Self::BottomLeft, Self::BottomMiddle, Self::BottomRight];

    fn key(self) -> &'static str {
        match self {
            Self::TopLeft => "mini-position-top-left",
            Self::TopMiddle => "mini-position-top-middle",
            Self::TopRight => "mini-position-top-right",
            Self::BottomLeft => "mini-position-bottom-left",
            Self::BottomMiddle => "mini-position-bottom-middle",
            Self::BottomRight => "mini-position-bottom-right",
        }
    }

    fn place(self, work: Bounds<Pixels>, edge: Pixels, gap: Pixels) -> Bounds<Pixels> {
        let left = work.left() + gap;
        let right = work.right() - gap - edge;
        let middle = work.center().x - edge / 2.;
        let x = match self {
            Self::TopLeft | Self::BottomLeft => left,
            Self::TopMiddle | Self::BottomMiddle => middle,
            Self::TopRight | Self::BottomRight => right,
        };
        let y = match self {
            Self::TopLeft | Self::TopMiddle | Self::TopRight => work.top() + gap,
            Self::BottomLeft | Self::BottomMiddle | Self::BottomRight => work.bottom() - gap - edge,
        };
        Bounds { origin: point(x, y), size: size(edge, edge) }
    }
}

/// An artwork-only companion window sharing the main player's playback and queue.
pub(crate) struct MiniPlayer {
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    settings: Entity<AppSettings>,
    parent: AnyWindowHandle,
    focus: FocusHandle,
    hovered: bool,
    hover_revision: usize,
    menu_open: bool,
    _bounds_watch: Subscription,
}

impl MiniPlayer {
    /// Restores the companion's own frame, recovering safely if its monitor was removed.
    pub(crate) fn open(
        parent: &mut Window,
        cx: &mut App,
    ) -> Option<WindowHandle<Self>> {
        let settings = Sonora::global(cx).settings.clone();
        let saved = settings.read(cx).mini_player_bounds();
        let display = saved.and_then(|bounds| cx.displays().into_iter()
            .find(|display| display.bounds().contains(&bounds.center())))
            .or_else(|| parent.display(cx));
        let display_id = display.as_ref().map(|display| display.id());
        let edge = saved.map(|bounds| bounds.size.width)
            .unwrap_or(cx.theme().metrics.cover * COVER_SCALE);
        let mut bounds = saved.unwrap_or_else(|| Bounds::centered(display_id, size(edge, edge), cx));
        if let Some(display) = display {
            bounds = fitted(bounds, display.visible_bounds(), cx);
        }
        let on_top = settings.read(cx).mini_player_on_top();
        let parent = parent.window_handle();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                titlebar: None,
                kind: WindowKind::PopUp,
                window_background: WindowBackgroundAppearance::Transparent,
                is_movable: true,
                is_resizable: false,
                is_minimizable: false,
                app_owns_titlebar_drag: true,
                app_id: Some("sonora".into()),
                ..Default::default()
            },
            move |window, cx| {
                apply_window(window, None, Some(on_top), cx);
                window.on_window_should_close(cx, move |_, cx| {
                    restore(parent, cx);
                    true
                });
                cx.new(|cx| {
                    let playback = Sonora::global(cx).playback.clone();
                    let queue = Sonora::global(cx).queue.clone();
                    let settings = Sonora::global(cx).settings.clone();
                    cx.observe(&playback, |_, _, cx| cx.notify()).detach();
                    cx.observe(&queue, |_, _, cx| cx.notify()).detach();
                    cx.observe(&settings, |_, _, cx| cx.notify()).detach();
                    let focus = cx.focus_handle();
                    window.focus(&focus, cx);
                    settings.update(cx, |settings, cx| settings.keep_mini_player_frame(window, cx));
                    let bounds_watch = cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                        this.settings.update(cx, |settings, cx| settings.keep_mini_player_frame(window, cx));
                    });
                    Self {
                        playback,
                        queue,
                        settings,
                        parent,
                        focus,
                        hovered: false,
                        hover_revision: 0,
                        menu_open: false,
                        _bounds_watch: bounds_watch,
                    }
                })
            },
        )
        .map_err(|error| log::error!("mini player: cannot open window: {error}"))
        .ok()
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |settings, cx| settings.keep_mini_player_frame(window, cx));
        restore(self.parent, cx);
        window.remove_window();
    }

    fn turn_volume(&mut self, event: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open { return; }
        let delta = event.delta.pixel_delta(window.line_height()).y;
        if delta == Pixels::ZERO { return; }
        cx.stop_propagation();
        let step = if delta > Pixels::ZERO { NOTCH } else { -NOTCH };
        let level = (self.playback.read(cx).volume() + step).clamp(0., 1.);
        self.playback.update(cx, |playback, cx| playback.set_volume(level, cx));
    }

    /// Run the native menu outside a GPUI update, since its message loop can dispatch redraws.
    #[cfg(target_os = "windows")]
    fn open_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use i18n::t;
        if self.menu_open { return; }
        let Some(owner) = native::handle(window) else { return; };
        let on_top = self.settings.read(cx).mini_player_on_top();
        let edge = window.viewport_size().width;
        let cover = cx.theme().metrics.cover;
        let positions = Position::ALL.iter().enumerate()
            .map(|(index, position)| ((index + 1) as u32, i18n::lookup(position.key(), None).to_string(), false))
            .collect();
        let sizes = ["mini-size-small", "mini-size-medium", "mini-size-large"]
            .into_iter().enumerate().map(|(index, key)| {
                ((index + 11) as u32, i18n::lookup(key, None).to_string(), (edge - cover * COVER_SCALES[index]).abs() < PIXEL)
            }).collect();
        let labels = native::Labels {
            position: t!("mini-position").to_string(),
            size: t!("mini-size").to_string(),
            on_top: t!("mini-on-top").to_string(),
            restore: t!("player-mini-restore").to_string(),
            positions, sizes,
        };
        self.menu_open = true;
        cx.spawn_in(window, async move |this, cx| {
            let command = native::popup(owner, labels, on_top);
            this.update_in(cx, |this, window, cx| {
                this.menu_open = false;
                this.hovered = false;
                this.hover_revision = this.hover_revision.wrapping_add(1);
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
            let enabled = !self.settings.read(cx).mini_player_on_top();
            self.settings.update(cx, |settings, cx| settings.set_mini_player_on_top(enabled, cx));
            apply_window(window, None, Some(enabled), cx);
            return;
        }
        if command == 22 { self.close(window, cx); return; }
        let Some(display) = window.display(cx) else { return; };
        let work = display.visible_bounds();
        let bounds = window.bounds();
        let next = match command {
            1..=6 => Position::ALL[(command - 1) as usize].place(work, bounds.size.width, cx.theme().metrics.inset),
            11..=13 => {
                let edge = cx.theme().metrics.cover * COVER_SCALES[(command - 11) as usize];
                // Keep the same center unless fitting back into the work area requires a move.
                Bounds { origin: bounds.center() - point(edge / 2., edge / 2.), size: size(edge, edge) }
            }
            _ => return,
        };
        apply_window(window, Some(fitted(next, work, cx)), None, cx);
    }
}

impl Render for MiniPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        window.set_rem_size(theme.font_size);
        let edge = window.viewport_size().width;
        let track = self.playback.read(cx).track();
        let cover = track.and_then(|track| track.cover.clone());
        let font = crate::root::ui_font(Sonora::global(cx).settings.read(cx).font());

        div()
            .id("mini-player")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .rounded(theme.radius)
            .overflow_hidden()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font(font)
            .on_scroll_wheel(cx.listener(Self::turn_volume))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if this.hovered == *hovered {
                    return;
                }
                this.hovered = *hovered;
                this.hover_revision = this.hover_revision.wrapping_add(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleMiniPlayer, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| this.close(window, cx)))
            .child(
                Artwork::new(cover)
                    .size(edge)
                    .corner_radius(theme.radius)
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_menu(window, cx);
                    })),
            )
            .when(self.hovered, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(theme.metrics.pad)
                        .right(theme.metrics.pad)
                        .rounded(theme.radius)
                        .bg(theme.background.opacity(0.78))
                        .child(
                            Button::new("mini-player-restore")
                                .ghost()
                                .small()
                                .icon("icons/maximize.svg")
                                .tooltip("player-mini-restore")
                                .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                        )
                        .with_animation(
                            ("mini-player-restore", self.hover_revision),
                            Motion::Control.animation(),
                            |this, progress| this.opacity(progress),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .bottom(theme.metrics.pad)
                        .left_0()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .p(theme.metrics.pad / 2.)
                                .rounded(theme.metrics.control)
                                .bg(theme.background.opacity(0.78))
                                .child(previous(&self.playback, false, cx))
                                .child(toggle(&self.playback, true, false, cx))
                                .child(next(&self.playback, &self.queue, false, cx)),
                        )
                        .with_animation(
                            ("mini-player-controls", self.hover_revision),
                            Motion::Control.animation(),
                            |this, progress| this.opacity(progress),
                        ),
                )
            })
    }
}

/// Keeps a square inside the usable screen, including after a monitor is removed.
fn fitted(bounds: Bounds<Pixels>, work: Bounds<Pixels>, cx: &App) -> Bounds<Pixels> {
    let theme = cx.theme();
    let gap = theme.metrics.inset.min(work.size.width.min(work.size.height) / 4.);
    let available = (work.size.width.min(work.size.height) - gap * 2.).max(PIXEL);
    let minimum = (theme.metrics.cover * COVER_SCALES[0]).min(available);
    let maximum = (theme.metrics.cover * COVER_SCALES[2]).min(available);
    let edge = bounds.size.width.clamp(minimum, maximum);
    let x = bounds.origin.x.clamp(work.left() + gap, work.right() - gap - edge);
    let y = bounds.origin.y.clamp(work.top() + gap, work.bottom() - gap - edge);
    Bounds { origin: point(x, y), size: size(edge, edge) }
}

/// Native changes run after GPUI releases the window to allow Windows messages through.
#[cfg(target_os = "windows")]
fn apply_window(window: &mut Window, bounds: Option<Bounds<Pixels>>, on_top: Option<bool>, cx: &mut App) {
    let Some(owner) = native::handle(window) else { return; };
    let scale = window.scale_factor();
    cx.foreground_executor().spawn(async move {
        native::apply(owner, bounds, scale, on_top);
    }).detach();
}

#[cfg(not(target_os = "windows"))]
fn apply_window(window: &mut Window, bounds: Option<Bounds<Pixels>>, _: Option<bool>, _: &mut App) {
    if let Some(bounds) = bounds { window.resize(bounds.size); }
}

fn restore(parent: AnyWindowHandle, cx: &mut App) {
    parent
        .update(cx, |_, window, _| window.activate_window())
        .ok();
}
