use gpui::prelude::*;
use gpui::{
    AnimationExt, AnyWindowHandle, App, Bounds, Context, Entity, FocusHandle, MouseButton, Render,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
    div, size,
};
use input::{CloseWindow, ToggleMiniPlayer, WORKSPACE_CONTEXT};
use state::{Playback, Queue, Sonora};
use ui::{ActiveTheme as _, Artwork, Button, Dismiss, Motion, Text};

use crate::shared::transport::{next, previous, toggle};

const COVER_SCALE: f32 = 2.3;

/// An artwork-only companion window sharing the main player's playback and queue.
pub(crate) struct MiniPlayer {
    playback: Entity<Playback>,
    queue: Entity<Queue>,
    parent: AnyWindowHandle,
    focus: FocusHandle,
    hovered: bool,
    hover_revision: usize,
}

impl MiniPlayer {
    /// Opens one fixed square above other apps without creating another playback engine.
    pub(crate) fn open(
        parent: &mut Window,
        cx: &mut App,
    ) -> Option<WindowHandle<Self>> {
        let edge = cx.theme().metrics.cover * COVER_SCALE;
        let display = parent.display(cx).map(|display| display.id());
        let parent = parent.window_handle();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    display,
                    size(edge, edge),
                    cx,
                ))),
                display_id: display,
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
                    Self {
                        playback,
                        queue,
                        parent,
                        focus,
                        hovered: false,
                        hover_revision: 0,
                    }
                })
            },
        )
        .map_err(|error| log::error!("mini player: cannot open window: {error}"))
        .ok()
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        restore(self.parent, cx);
        window.remove_window();
    }
}

impl Render for MiniPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.theme();
        window.set_rem_size(theme.font_size);
        let edge = window.viewport_size().width;
        let track = self.playback.read(cx).track();
        let cover = track.and_then(|track| track.cover.clone());
        let title = track.map(|track| track.name.clone());
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
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move()),
            )
            .when(self.hovered, |this| {
                this.child(
                    div()
                        .absolute()
                        .top(theme.metrics.pad)
                        .right(theme.metrics.pad)
                        .rounded(theme.radius)
                        .bg(theme.background.opacity(0.9))
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
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .p(theme.metrics.pad)
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .bg(theme.background.opacity(0.9))
                        .rounded_b(theme.radius)
                        .when_some(title, |this, title| {
                            this.child(
                                div()
                                    .w_full()
                                    .text_center()
                                    .text_size(theme.text(Text::Small))
                                    .truncate()
                                    .child(title),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
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

fn restore(parent: AnyWindowHandle, cx: &mut App) {
    parent
        .update(cx, |_, window, _| window.activate_window())
        .ok();
}
