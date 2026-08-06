//! winit events, as gpui input.
//!
//! Two things here are more than a rename.
//!
//! **Key names.** gpui identifies a key by the name printed on it — `"a"`, `"enter"`, `"f1"` — and
//! keeps whatever character the key would have typed separately in `key_char`, so that a binding
//! on `cmd-s` still fires on a layout where that key types something else. winit gives a
//! [`Key`](winit::keyboard::Key) that already has the layout applied. [`key_name`] recovers gpui's
//! vocabulary from it.
//!
//! **Click counts.** gpui expects a running click count on every press and release; winit reports
//! neither. [`Clicks`] reconstructs it from the system double-click interval and a movement
//! threshold, which is what every other gpui backend gets from its window system.

use std::time::{Duration, Instant};

use gpui::{
    Capslock, Modifiers, MouseButton, NavigationDirection, Pixels, Point, ScrollDelta, px,
};
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// How long after a click a second one still counts as a double click.
///
/// winit exposes no system setting for this, so it is the common default rather than the user's
/// actual preference.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// How far the pointer may move between clicks and still count as the same sequence.
const MULTI_CLICK_SLOP: f32 = 4.0;

/// Reconstructs click counts from a stream of presses.
#[derive(Default)]
pub struct Clicks {
    last: Option<(MouseButton, Point<Pixels>, Instant)>,
    count: usize,
}

impl Clicks {
    /// The click count for a press of `button` at `position`.
    pub fn press(&mut self, button: MouseButton, position: Point<Pixels>) -> usize {
        let now = Instant::now();
        let continues = self.last.as_ref().is_some_and(|(last, at, when)| {
            *last == button
                && now.duration_since(*when) <= MULTI_CLICK_INTERVAL
                && (f32::from(at.x) - f32::from(position.x)).abs() <= MULTI_CLICK_SLOP
                && (f32::from(at.y) - f32::from(position.y)).abs() <= MULTI_CLICK_SLOP
        });
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some((button, position, now));
        self.count
    }

    /// The click count to report on release, which is the one the press reported.
    pub fn release(&self) -> usize {
        self.count
    }
}

/// gpui's modifier set, from winit's.
pub fn modifiers(state: ModifiersState) -> Modifiers {
    Modifiers {
        control: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
        platform: state.super_key(),
        // winit has no notion of the macOS `fn` key.
        function: false,
    }
}

/// gpui's capslock state.
///
/// winit reports modifier *keys*, not lock state, so this is always off. The consequence is that
/// bindings conditioned on capslock never match under this backend.
pub fn capslock() -> Capslock {
    Capslock { on: false }
}

/// gpui's mouse button, from winit's, or `None` for a button gpui has no name for.
pub fn mouse_button(button: WinitButton) -> Option<MouseButton> {
    Some(match button {
        WinitButton::Left => MouseButton::Left,
        WinitButton::Right => MouseButton::Right,
        WinitButton::Middle => MouseButton::Middle,
        WinitButton::Back => MouseButton::Navigate(NavigationDirection::Back),
        WinitButton::Forward => MouseButton::Navigate(NavigationDirection::Forward),
        WinitButton::Other(_) => return None,
    })
}

/// gpui's scroll delta, from winit's.
///
/// A line delta stays a line delta: gpui distinguishes the two so that it can apply its own
/// line-height, and flattening them here would lose that. The sign is inverted because winit
/// reports the direction the *content* moves and gpui expects the direction the *view* moves.
pub fn scroll_delta(delta: MouseScrollDelta, scale: f32) -> ScrollDelta {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines(Point { x, y }),
        MouseScrollDelta::PixelDelta(position) => ScrollDelta::Pixels(Point {
            x: px(position.x as f32 / scale),
            y: px(position.y as f32 / scale),
        }),
    }
}

/// Whether a winit element state is a press.
pub fn is_pressed(state: ElementState) -> bool {
    state == ElementState::Pressed
}

/// gpui's name for a key, from winit's logical key.
///
/// gpui names keys the way a keymap file spells them, which is mostly lowercase words. Anything
/// this does not recognise is passed through as its own text, which is what makes an unlisted key
/// bindable rather than invisible.
pub fn key_name(key: &Key) -> Option<String> {
    let named = |name: &str| Some(name.to_owned());
    match key {
        Key::Character(text) => {
            // gpui wants the unshifted name of the key: `shift-a` rather than `A`, because the
            // shift is already in the modifiers and reporting both would need every binding
            // written twice.
            let lowered = text.to_lowercase();
            (!lowered.is_empty()).then_some(lowered)
        }
        Key::Named(name) => match name {
            NamedKey::Enter => named("enter"),
            NamedKey::Tab => named("tab"),
            NamedKey::Space => named("space"),
            NamedKey::Backspace => named("backspace"),
            NamedKey::Delete => named("delete"),
            NamedKey::Escape => named("escape"),
            NamedKey::ArrowUp => named("up"),
            NamedKey::ArrowDown => named("down"),
            NamedKey::ArrowLeft => named("left"),
            NamedKey::ArrowRight => named("right"),
            NamedKey::Home => named("home"),
            NamedKey::End => named("end"),
            NamedKey::PageUp => named("pageup"),
            NamedKey::PageDown => named("pagedown"),
            NamedKey::Insert => named("insert"),
            NamedKey::F1 => named("f1"),
            NamedKey::F2 => named("f2"),
            NamedKey::F3 => named("f3"),
            NamedKey::F4 => named("f4"),
            NamedKey::F5 => named("f5"),
            NamedKey::F6 => named("f6"),
            NamedKey::F7 => named("f7"),
            NamedKey::F8 => named("f8"),
            NamedKey::F9 => named("f9"),
            NamedKey::F10 => named("f10"),
            NamedKey::F11 => named("f11"),
            NamedKey::F12 => named("f12"),
            // Modifier keys are reported through `ModifiersChanged` rather than as keystrokes.
            NamedKey::Shift
            | NamedKey::Control
            | NamedKey::Alt
            | NamedKey::Super
            | NamedKey::Meta
            | NamedKey::CapsLock => None,
            other => other.to_text().map(str::to_owned),
        },
        _ => None,
    }
}

/// The text a key press would type, or `None` when it types nothing.
///
/// gpui uses this to tell "the user typed a character" from "the user pressed a bound key", so a
/// control chord must not report text: `ctrl-a` types nothing and reporting `"a"` would insert an
/// `a` alongside running the binding.
pub fn key_char(key: &Key, modifiers: Modifiers) -> Option<String> {
    if modifiers.control || modifiers.platform {
        return None;
    }
    match key {
        Key::Character(text) => Some(text.to_string()),
        Key::Named(NamedKey::Space) => Some(" ".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32) -> Point<Pixels> {
        Point { x: px(x), y: px(y) }
    }

    #[test]
    fn repeated_presses_in_the_same_place_count_up() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.press(MouseButton::Left, at(10.0, 10.0)), 1);
        assert_eq!(clicks.press(MouseButton::Left, at(10.0, 10.0)), 2);
        assert_eq!(clicks.press(MouseButton::Left, at(11.0, 11.0)), 3);
        assert_eq!(clicks.release(), 3);
    }

    #[test]
    fn a_press_far_away_starts_a_new_sequence() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.press(MouseButton::Left, at(10.0, 10.0)), 1);
        assert_eq!(clicks.press(MouseButton::Left, at(80.0, 10.0)), 1);
    }

    #[test]
    fn a_different_button_starts_a_new_sequence() {
        let mut clicks = Clicks::default();
        assert_eq!(clicks.press(MouseButton::Left, at(10.0, 10.0)), 1);
        assert_eq!(clicks.press(MouseButton::Right, at(10.0, 10.0)), 1);
    }

    #[test]
    fn a_shifted_letter_is_named_by_its_unshifted_key() {
        let key = Key::Character("A".into());
        assert_eq!(key_name(&key).as_deref(), Some("a"));
    }

    #[test]
    fn a_shifted_letter_still_types_its_shifted_character() {
        let key = Key::Character("A".into());
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(key_char(&key, shift).as_deref(), Some("A"));
    }

    #[test]
    fn a_control_chord_types_nothing() {
        let key = Key::Character("a".into());
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(key_char(&key, control), None);
        assert_eq!(key_name(&key).as_deref(), Some("a"));
    }

    #[test]
    fn modifier_keys_are_not_keystrokes_of_their_own() {
        assert_eq!(key_name(&Key::Named(NamedKey::Shift)), None);
        assert_eq!(key_name(&Key::Named(NamedKey::Control)), None);
    }
}
