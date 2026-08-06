//! Everything the backend translates, in one window.
//!
//! Rounded quads with borders, a gradient, a drop shadow, a path drawn through `canvas`, a
//! content-filter blur group and a frosted backdrop panel — each of which takes a different route
//! through the translation. A regression in any one of them is visible here without reading a log.
//!
//! The moving square is what makes damage-based redraw observable: only the strip it travels
//! through is repainted each frame, while everything else keeps the pixels it already had.
//!
//! Run with `cargo run -p gpui_zgui --example showcase`.

use std::rc::Rc;

use gpui::{
    AppContext as _, Application, Bounds, Context, Hsla, IntoElement, ParentElement, Path, Pixels,
    Point, Render, Styled, Window, WindowOptions, canvas, div, linear_color_stop, linear_gradient,
    px, rgb,
};

struct Showcase {
    /// Advances every frame, so something is always changing and damage is always non-empty.
    tick: f32,
}

impl Render for Showcase {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick += 0.01;
        if self.tick > 1.0 {
            self.tick = 0.0;
        }
        // Asking for another frame is what keeps the animation running; gpui decides whether the
        // frame actually redraws anything.
        cx.notify();
        let travel = self.tick;

        div()
            .size_full()
            .bg(rgb(0x11111b))
            .flex()
            .flex_col()
            .gap_5()
            .p_8()
            .child(
                div()
                    .flex()
                    .gap_5()
                    .child(
                        // Rounded quad with a border.
                        div()
                            .w(px(160.0))
                            .h(px(90.0))
                            .rounded(px(14.0))
                            .bg(rgb(0x89b4fa))
                            .border_4()
                            .border_color(rgb(0xf5c2e7)),
                    )
                    .child(
                        // A gradient, which takes the paint-table route.
                        div().w(px(160.0)).h(px(90.0)).rounded(px(14.0)).bg(
                            linear_gradient(
                                135.0,
                                linear_color_stop(rgb(0xf38ba8), 0.0),
                                linear_color_stop(rgb(0xfab387), 1.0),
                            ),
                        ),
                    )
                    .child(
                        // A drop shadow, whose painted extent is three sigma wider than its box.
                        div()
                            .w(px(160.0))
                            .h(px(90.0))
                            .rounded(px(14.0))
                            .bg(rgb(0xa6e3a1))
                            .shadow_lg(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_5()
                    .child(
                        // A content-filter group: the whole subtree is isolated and blurred.
                        div()
                            .w(px(160.0))
                            .h(px(90.0))
                            .rounded(px(14.0))
                            .bg(rgb(0xcba6f7))
                            .blur(px(6.0)),
                    )
                    .child(
                        // A path, tessellated by gpui and rasterised into a coverage mask here.
                        div()
                            .w(px(160.0))
                            .h(px(90.0))
                            .child(canvas(|_, _, _| (), |bounds, _, window, _| {
                                window.paint_path(chevron(bounds), rgb(0x94e2d5));
                            })),
                    ),
            )
            .child(
                // A strip the square travels along, so damage is visibly a moving band.
                div().h(px(60.0)).w_full().relative().child(
                    div()
                        .absolute()
                        .left(px(travel * 420.0))
                        .top(px(10.0))
                        .w(px(40.0))
                        .h(px(40.0))
                        .rounded(px(8.0))
                        .bg(rgb(0xf9e2af)),
                ),
            )
            .child(
                // A frosted panel over a busy background: the backdrop filter reads what is
                // beneath it, so its damage has to grow to whatever it samples.
                div()
                    .relative()
                    .h(px(90.0))
                    .w_full()
                    .bg(linear_gradient(
                        90.0,
                        linear_color_stop(rgb(0x74c7ec), 0.0),
                        linear_color_stop(rgb(0xf5c2e7), 1.0),
                    ))
                    .child(
                        div()
                            .absolute()
                            .left(px(40.0))
                            .top(px(20.0))
                            .w(px(240.0))
                            .h(px(50.0))
                            .rounded(px(12.0))
                            .bg(Hsla {
                                h: 0.0,
                                s: 0.0,
                                l: 1.0,
                                a: 0.15,
                            })
                            .backdrop_filter(vec![gpui::Filter::Blur(px(8.0))]),
                    ),
            )
    }
}

/// A chevron, built through the path API so it exercises both straight and curved segments.
fn chevron(bounds: Bounds<Pixels>) -> Path<Pixels> {
    let at = |x: f32, y: f32| Point {
        x: bounds.origin.x + px(x),
        y: bounds.origin.y + px(y),
    };
    let mut path = Path::new(at(20.0, 20.0));
    path.line_to(at(70.0, 45.0));
    path.line_to(at(20.0, 70.0));
    path.line_to(at(34.0, 45.0));
    path.curve_to(at(20.0, 20.0), at(24.0, 30.0));
    path
}

fn main() {
    env_logger::init();
    Application::with_platform(Rc::new(gpui_zgui::ZguiPlatform::new())).run(|cx| {
        cx.open_window(WindowOptions::default(), |_window, cx| {
            cx.new(|_| Showcase { tick: 0.0 })
        })
        .expect("opening a window");
    });
}
