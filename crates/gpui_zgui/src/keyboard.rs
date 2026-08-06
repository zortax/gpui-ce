//! Keyboard layout reporting.
//!
//! winit resolves a key press to a logical key for us and exposes no layout identity of its own,
//! so there is nothing here to interrogate: the layout is reported as a single unnamed one, and
//! keystroke mapping falls through to gpui's own [`DummyKeyboardMapper`](gpui::DummyKeyboardMapper).
//!
//! The consequence is that `use_key_equivalents` bindings — the macOS mechanism for keeping
//! shortcuts on the same physical keys across layouts — behave as they do on gpui's other
//! non-macOS backends, which is to say they are the identity.

use gpui::PlatformKeyboardLayout;

/// The one layout this backend can describe.
pub struct ZguiKeyboardLayout;

impl PlatformKeyboardLayout for ZguiKeyboardLayout {
    fn id(&self) -> &str {
        "winit"
    }

    fn name(&self) -> &str {
        "System"
    }
}
