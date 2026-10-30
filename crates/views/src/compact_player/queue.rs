//! An attached queue drawer. Only its collapsed frame is persisted between launches.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{AnyElement, Bounds, Context, Entity, Pixels, SharedString, UniformListScrollHandle, Window, div, point, size, uniform_list};
use i18n::t;
use ui::{ActiveTheme as _, Button, Card, Motion, Scrollbar, Scroller, Text, clock};

use super::motion::Scalar;
use super::{CompactPlayer, PIXEL, apply, dimensions};

const QUEUE_ROWS: f32 = 4.5;
const PANEL_ZOOM: f32 = 0.025;

/// The card's resting rectangle stays independent of the animated native window height.
pub(super) struct Drawer {
    pub(super) open: bool,
    pub(super) above: bool,
    pub(super) base: Bounds<Pixels>,
    height: Pixels,
    reveal: Scalar,
    scroll: UniformListScrollHandle,
    scrollbar: Entity<Scrollbar>,
}

impl Drawer {
    pub(super) fn new(base: Bounds<Pixels>, cx: &mut Context<CompactPlayer>) -> Self {
        let scroll = UniformListScrollHandle::new();
        let view = cx.entity_id();
        let scrollbar = cx.new(|_| Scrollbar::new(scroll.0.borrow().base_handle.clone()).watching(view));
        Self { open: false, above: false, base, height: Pixels::ZERO, reveal: Scalar::new(0.), scroll, scrollbar }
    }
}

impl CompactPlayer {
    /// Maps moves and DPI changes back to the card, never saving the drawer's temporary height.
    pub(super) fn keep_card_frame(&mut self, window: &Window, cx: &mut Context<Self>) {
        let bounds = window.bounds();
        let scale = bounds.size.width / dimensions(1., cx).width;
        let height = dimensions(scale, cx).height;
        let extension = (bounds.size.height - height).max(Pixels::ZERO);
        let y = bounds.origin.y + match self.drawer.above { true => extension, false => Pixels::ZERO };
        self.drawer.base = Bounds {
            origin: point(bounds.origin.x, y),
            size: size(bounds.size.width, height),
        };
        let base = self.drawer.base;
        self.settings.update(cx, |settings, cx| settings.keep_compact_player_bounds(base, cx));
    }

    /// Chooses the roomier direction without moving the resting card or opening the main app.
    pub(super) fn toggle_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.drawer.open && self.drawer.height == Pixels::ZERO {
            self.keep_card_frame(window, cx);
            let theme = *cx.theme();
            let scale = self.drawer.base.size.width / dimensions(1., cx).width;
            let wanted = (theme.metrics.list_row * QUEUE_ROWS + theme.metrics.control + theme.metrics.pad * 3.) * scale;
            let work = window.display(cx).map(|display| display.visible_bounds());
            let (above, available) = match work {
                Some(work) => {
                    let below = (work.bottom() - theme.metrics.inset - self.drawer.base.bottom()).max(Pixels::ZERO);
                    let above = (self.drawer.base.top() - work.top() - theme.metrics.inset).max(Pixels::ZERO);
                    let upwards = below < wanted && above > below;
                    (upwards, match upwards { true => above, false => below })
                }
                None => (false, wanted),
            };
            self.drawer.above = above;
            self.drawer.height = ui::snapped(wanted.min(available), window);
        }
        self.drawer.open = !self.drawer.open;
        cx.notify();
    }

    /// Menu alignment and sizing first return to the ordinary card geometry.
    pub(super) fn collapse_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drawer.open = false;
        self.drawer.reveal = Scalar::new(0.);
        self.drawer.height = Pixels::ZERO;
        apply(window, Some(self.drawer.base), None, cx);
    }

    /// Eases the native client area and panel together, scheduling frames only during motion.
    pub(super) fn animate_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let mix = self.drawer.reveal.towards(f32::from(u8::from(self.drawer.open)), Motion::Slow, window, cx);
        let extra = ui::snapped(self.drawer.height * mix, window);
        let mut target = self.drawer.base;
        target.size.height += extra;
        if self.drawer.above { target.origin.y -= extra; }
        let actual = window.bounds();
        let tolerance = PIXEL / (window.scale_factor() * 2.);
        if (actual.size.height - target.size.height).abs() > tolerance
            || (actual.origin.y - target.origin.y).abs() > tolerance {
            apply(window, Some(target), None, cx);
        }
        if !self.drawer.open && mix == 0. { self.drawer.height = Pixels::ZERO; }
        mix
    }

    /// Virtualized rows follow the existing queue, including autoplay suggestions.
    pub(super) fn queue_panel(&self, mix: f32, window: &Window, cx: &Context<Self>) -> AnyElement {
        let visible = (window.viewport_size().height - self.drawer.base.size.height).max(Pixels::ZERO);
        if visible <= Pixels::ZERO { return div().h_0().flex_none().into_any_element(); }
        let theme = *cx.theme();
        let queue = self.queue.read(cx);
        let total = queue.upcoming().len();
        let revision = queue.revision();
        let scroll = self.drawer.scroll.clone();
        self.drawer.scrollbar.read(cx).sync();

        let rows = uniform_list("compact-queue-rows", total, cx.processor(move |this, range: Range<usize>, _, cx| {
            let queue = this.queue.read(cx);
            let entries = queue.upcoming().enumerate()
                .skip(range.start).take(range.len())
                .filter_map(|(index, track)| queue.uid(state::QueuePosition::Upcoming(index))
                    .map(|uid| (track.clone(), uid.to_owned())))
                .collect::<Vec<_>>();
            entries.into_iter().enumerate().map(|(offset, (track, uid))| {
                let index = range.start + offset;
                let playable = track.playable;
                Card::new(("compact-queue-track", index), SharedString::from(track.name))
                    .cover(track.cover).art(theme.metrics.control).art_radius(theme.radius / 2.)
                    .size(Text::Small).weight(gpui::FontWeight::MEDIUM)
                    .tint(theme.popover_foreground)
                    .bare_meta(div().min_w_0().truncate().text_size(theme.text(Text::Tiny))
                        .text_color(theme.popover_foreground.opacity(0.6)).child(SharedString::from(track.artists)))
                    .when(track.explicit, Card::explicit)
                    .trailing(div().text_size(theme.text(Text::Tiny)).text_color(theme.popover_foreground.opacity(0.5))
                        .child(clock(track.duration)))
                    .h(theme.metrics.list_row).w_full().min_w_0()
                    .hover(move |style| style.bg(theme.popover_foreground.opacity(0.08)))
                    .when(playable, |card| card.press(cx.listener(move |this, _, _, cx| {
                        if this.queue.read(cx).revision() != revision { return; }
                        this.playback.update(cx, |playback, cx| playback.play_queue(&uid, cx));
                    })))
                    .into_any_element()
            }).collect()
        })).track_scroll(&scroll).size_full();

        let heading = div().flex().items_center().justify_between().flex_none().px_2()
            .h(theme.metrics.control + theme.metrics.pad)
            .child(div().flex().items_center().gap_2().min_w_0()
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(t!("queue-up-next")))
                .child(div().text_size(theme.text(Text::Tiny)).text_color(theme.popover_foreground.opacity(0.5))
                    .child(t!("compact-queue-count", count = total))))
            .child(Button::new("compact-queue-collapse").ghost().backgroundless().small().icon("icons/x.svg")
                .tooltip("compact-queue-close").on_click(cx.listener(|this, _, window, cx| this.toggle_queue(window, cx))));

        div().h(visible).w_full().flex_none().overflow_hidden()
            .child(div().h(self.drawer.height.max(visible)).w_full().flex().flex_col().px_2().pb_2()
                .opacity(mix).layer_scale(1. - PANEL_ZOOM * (1. - mix))
                .border_color(theme.border.opacity(0.5))
                .when_else(self.drawer.above, |this| this.border_b_1(), |this| this.border_t_1())
                .child(heading)
                .child(div().flex_1().min_h_0().relative()
                    .when_else(total == 0,
                        |this| this.flex().items_center().justify_center().text_size(theme.text(Text::Small))
                            .text_color(theme.popover_foreground.opacity(0.55)).child(t!("queue-empty")),
                        |this| this.child(Scroller::listing("compact-queue-scroll", &self.drawer.scrollbar).size_full().child(rows)))))
            .into_any_element()
    }
}
