//! Finite compact-player feedback. Settled controls schedule no animation frames.

use std::time::{Duration, Instant};

use gpui::{App, Window};
use ui::Motion;

const COUNT: usize = 7;
const HOVER_LIFT: f32 = 0.04;
const CLICK_DIP: f32 = 0.12;
const HOVER_FILL: f32 = 0.09;

/// Stable identities keep feedback alive when play changes to pause or an icon changes.
#[derive(Clone, Copy)]
pub(super) enum Control {
    Volume,
    Options,
    Previous,
    Playback,
    Next,
    Lyrics,
    Queue,
}

/// Retargets from the current drawn value, so rapid changes never restart from an endpoint.
struct Scalar {
    from: f32,
    to: f32,
    started: Instant,
    span: Duration,
}

impl Scalar {
    fn new(value: f32) -> Self {
        Self { from: value, to: value, started: Instant::now(), span: Duration::ZERO }
    }

    fn sample(&self, now: Instant) -> f32 {
        if self.span.is_zero() { return self.to; }
        let progress = (now.duration_since(self.started).as_secs_f32() / self.span.as_secs_f32()).clamp(0., 1.);
        self.from + (self.to - self.from) * ui::ease_out_cubic(progress)
    }

    fn towards(&mut self, target: f32, motion: Motion, window: &Window, cx: &App) -> f32 {
        if !ui::motion::animates(cx) {
            self.from = target;
            self.to = target;
            self.span = Duration::ZERO;
            return target;
        }
        let now = Instant::now();
        if target != self.to {
            self.from = self.sample(now);
            self.to = target;
            self.started = now;
            self.span = motion.span();
        }
        if now.duration_since(self.started) < self.span { window.request_animation_frame(); }
        self.sample(now)
    }
}

/// Owns local visual state without delaying or changing any playback command.
pub(super) struct Feedback {
    playback: Scalar,
    hovered: [bool; COUNT],
    hover: [Scalar; COUNT],
    clicked: [Option<Instant>; COUNT],
}

impl Feedback {
    pub(super) fn new(playing: bool) -> Self {
        Self {
            playback: Scalar::new(f32::from(u8::from(playing))),
            hovered: [false; COUNT],
            hover: std::array::from_fn(|_| Scalar::new(0.)),
            clicked: [None; COUNT],
        }
    }

    pub(super) fn hover(&mut self, control: Control, hovered: bool) {
        self.hovered[control as usize] = hovered;
    }

    pub(super) fn tap(&mut self, control: Control) {
        self.clicked[control as usize] = Some(Instant::now());
    }

    pub(super) fn playback(&mut self, playing: bool, window: &Window, cx: &App) -> f32 {
        self.playback.towards(f32::from(u8::from(playing)), Motion::Base, window, cx)
    }

    /// Hover eases in and out; a click compresses and releases once. Neither animation repeats.
    pub(super) fn button(&mut self, control: Control, enabled: bool, window: &Window, cx: &App) -> (f32, f32) {
        let index = control as usize;
        let hover = self.hover[index].towards(f32::from(u8::from(enabled && self.hovered[index])), Motion::Quick, window, cx);
        let pulse = match self.clicked[index] {
            Some(at) if enabled && ui::motion::animates(cx) && at.elapsed() < Motion::Control.span() => {
                window.request_animation_frame();
                let progress = (at.elapsed().as_secs_f32() / Motion::Control.span().as_secs_f32()).clamp(0., 1.);
                (std::f32::consts::PI * progress).sin()
            }
            _ => { self.clicked[index] = None; 0. }
        };
        (1. + HOVER_LIFT * hover - CLICK_DIP * pulse, HOVER_FILL * hover)
    }
}
