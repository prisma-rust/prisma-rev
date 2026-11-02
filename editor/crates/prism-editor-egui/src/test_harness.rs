#![allow(dead_code)] // Helper surface: key_press/context stay available for future scenarios.

//! Headless egui interaction harness.
//!
//! `EguiHarness` drives a real [`egui::Context`] with scripted [`egui::RawInput`]
//! events. It needs no window, GPU or `DISPLAY`: the draw closure runs inside
//! `Context::run`, exactly like the editor panels do in production.
//!
//! Tests use it to type into text fields, click toolbar buttons and drag anchor
//! handles with explicit coordinates and a deterministic frame sequence.

use egui::{Context, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect};

/// A deterministic pointer + keyboard script driver.
pub struct EguiHarness {
    ctx: Context,
    screen: Rect,
    time: f64,
    pointer: Pos2,
    modifiers: Modifiers,
    events: Vec<Event>,
}

impl Default for EguiHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl EguiHarness {
    pub fn new() -> Self {
        Self::with_size(800.0, 600.0)
    }

    pub fn with_size(width: f32, height: f32) -> Self {
        assert!(width > 0.0 && height > 0.0, "screen must be positive");
        Self {
            ctx: Context::default(),
            screen: Rect::from_min_size(Pos2::ZERO, egui::vec2(width, height)),
            time: 0.0,
            pointer: egui::pos2(width * 0.5, height * 0.5),
            modifiers: Modifiers::NONE,
            events: Vec::new(),
        }
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    /// Run one frame, delivering the queued events to `draw`.
    pub fn frame(&mut self, draw: &mut impl FnMut(&Context)) {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.time),
            modifiers: self.modifiers,
            events: std::mem::take(&mut self.events),
            ..RawInput::default()
        };
        let _ = self.ctx.run(input, |ctx| draw(ctx));
    }

    /// Queue a pointer move and advance one frame.
    pub fn move_to(&mut self, position: Pos2, draw: &mut impl FnMut(&Context)) {
        self.pointer = position;
        self.events.push(Event::PointerMoved(position));
        self.frame(draw);
    }

    /// Move to `position` and click the primary button.
    ///
    /// Three frames: hover, press, release. That is the same sequence egui sees
    /// from a real mouse and makes `Response::clicked()` fire on the release.
    pub fn click(&mut self, position: Pos2, draw: &mut impl FnMut(&Context)) {
        self.move_to(position, draw);
        self.press(PointerButton::Primary, draw);
        self.release(PointerButton::Primary, draw);
    }

    /// Press a button at the current pointer position and advance one frame.
    pub fn press(&mut self, button: PointerButton, draw: &mut impl FnMut(&Context)) {
        self.events.push(Event::PointerButton {
            pos: self.pointer,
            button,
            pressed: true,
            modifiers: self.modifiers,
        });
        self.frame(draw);
    }

    /// Release a button at the current pointer position and advance one frame.
    pub fn release(&mut self, button: PointerButton, draw: &mut impl FnMut(&Context)) {
        self.events.push(Event::PointerButton {
            pos: self.pointer,
            button,
            pressed: false,
            modifiers: self.modifiers,
        });
        self.frame(draw);
    }

    /// Press at `from`, move through `steps` interpolated positions, release at
    /// `to`. Every position is delivered on its own frame.
    pub fn drag(&mut self, from: Pos2, to: Pos2, steps: usize, draw: &mut impl FnMut(&Context)) {
        assert!(steps > 0, "drag needs at least one move");
        self.move_to(from, draw);
        self.press(PointerButton::Primary, draw);
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let position = egui::pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
            self.move_to(position, draw);
        }
        self.release(PointerButton::Primary, draw);
    }

    /// Deliver one `Event::Text` (as if typed) and advance one frame.
    pub fn text(&mut self, text: &str, draw: &mut impl FnMut(&Context)) {
        self.events.push(Event::Text(text.to_string()));
        self.frame(draw);
    }

    /// Press and release a key, with modifiers held for both events.
    pub fn key_press(&mut self, key: Key, modifiers: Modifiers, draw: &mut impl FnMut(&Context)) {
        self.modifiers = modifiers;
        for pressed in [true, false] {
            self.events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers,
            });
            self.frame(draw);
        }
        self.modifiers = Modifiers::NONE;
    }
}
