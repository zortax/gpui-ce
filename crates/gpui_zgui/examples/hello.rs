//! An ordinary gpui application, drawn by zgui.
//!
//! Nothing here is specific to this backend except the one line that constructs the platform.
//! That is the point of the exercise: the element tree, the styling DSL, `Entity<T>`, `Render` and
//! the window semantics are all gpui's own.
//!
//! Run with `cargo run -p gpui_zgui --example hello`.

use std::rc::Rc;

use gpui::{
    AppContext as _, Application, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowOptions, div, px, rgb,
};

struct Hello;

impl Render for Hello {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .justify_center()
            .items_center()
            .bg(rgb(0x1e1e2e))
            .child(
                div()
                    .w(px(320.0))
                    .h(px(120.0))
                    .rounded(px(16.0))
                    .bg(rgb(0x89b4fa))
                    .border_4()
                    .border_color(rgb(0xf5c2e7)),
            )
            .child(
                div()
                    .w(px(220.0))
                    .h(px(60.0))
                    .rounded(px(30.0))
                    .bg(rgb(0xa6e3a1)),
            )
    }
}

fn main() {
    env_logger::init();
    Application::with_platform(Rc::new(gpui_zgui::ZguiPlatform::new())).run(|cx| {
        cx.open_window(WindowOptions::default(), |_window, cx| cx.new(|_| Hello))
            .expect("opening a window");
    });
}
