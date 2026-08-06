//! An experimental gpui backend that draws through zgui.
//!
//! gpui's own renderers clear and redraw the whole window every frame. zgui's renderer composes
//! into a target it keeps between frames and scissors each frame's passes to a damage set, so
//! unchanged regions cost nothing. This crate is the seam between the two: it implements gpui's
//! platform contract on winit and gpui's renderer contract on zgui, without changing anything an
//! application sees.
//!
//! Nothing here is visible to a gpui application. The same `Entity<T>`, `Render`, `div()` and
//! window semantics apply; only what happens after [`gpui::PlatformWindow::draw`] differs.
//!
//! ```no_run
//! # fn main() {
//! gpui::Application::with_platform(std::rc::Rc::new(gpui_zgui::ZguiPlatform::new()))
//!     .run(|_cx| { /* an ordinary gpui app */ });
//! # }
//! ```

pub use crate::atlas::ZguiAtlas;
pub use crate::renderer::{Unsupported, ZguiRenderer};
pub use crate::display::ZguiDisplay;
pub use crate::platform::ZguiPlatform;
pub use crate::window::ZguiWindow;

mod atlas;
mod convert;
mod dispatcher;
mod display;
mod input;
mod keyboard;
mod platform;
mod renderer;
mod window;
