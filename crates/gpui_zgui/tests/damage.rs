//! Damage-based redraw must be invisible.
//!
//! The renderer composes into a target it keeps between frames and redraws only the rectangles it
//! believes changed. That is only sound if the belief is right, and a wrong belief shows up as
//! stale pixels — the previous frame's content left behind in a region that should have been
//! repainted. It is the kind of bug that looks like nothing at all until a specific interaction
//! hits it.
//!
//! So every test here draws the same sequence of frames twice: once through a renderer redrawing
//! only what changed, and once through one redrawing everything. The two must agree pixel for
//! pixel. That is the strongest statement available — it does not check that damage is *small*,
//! only that restricting the redraw never changed the picture.
//!
//! `damage_actually_restricts_the_redraw` covers the other half, so that a damage set which
//! silently degraded to "everything" could not pass the whole file.

use gpui::{
    Background, Bounds, ContentMask, Corners, Edges, Hsla, Point, ScaledPixels, Scene, Size,
};
use gpui_zgui::ZguiRenderer;
use zgui_geom::Size as ZSize;
use zgui_render_wgpu::wgpu;

const SIDE: i32 = 64;

fn renderer(incremental: bool) -> Option<ZguiRenderer> {
    match ZguiRenderer::offscreen(ZSize::new(SIDE, SIDE), 1.0, wgpu::TextureFormat::Bgra8Unorm) {
        Ok(mut renderer) => {
            renderer.set_incremental(incremental);
            Some(renderer)
        }
        Err(error) => {
            eprintln!("skipped: {error:#}");
            None
        }
    }
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: Point {
            x: ScaledPixels(x),
            y: ScaledPixels(y),
        },
        size: Size {
            width: ScaledPixels(width),
            height: ScaledPixels(height),
        },
    }
}

fn full_mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(0.0, 0.0, SIDE as f32, SIDE as f32),
    }
}

fn rgb(r: f32, g: f32, b: f32) -> Hsla {
    gpui::Rgba { r, g, b, a: 1.0 }.into()
}

fn quad(at: Bounds<ScaledPixels>, background: impl Into<Background>) -> gpui::Quad {
    gpui::Quad {
        order: 0,
        border_style: gpui::BorderStyle::Solid,
        bounds: at,
        content_mask: full_mask(),
        background: background.into(),
        border_color: Hsla::default(),
        corner_radii: Corners::default(),
        border_widths: Edges::default(),
    }
}

/// Builds a scene from a list of quads.
fn scene_of(quads: Vec<gpui::Quad>) -> Scene {
    let mut scene = Scene::default();
    for quad in quads {
        scene.insert_primitive(quad);
    }
    scene.finish();
    scene
}

/// Draws `frames` in order through both renderers and asserts they agree after every one.
///
/// Checking after *every* frame rather than only the last is deliberate: a damage bug that leaves
/// stale pixels is often corrected by the next frame, so comparing only the end state would miss
/// it entirely.
#[track_caller]
fn agrees_frame_by_frame(frames: Vec<Vec<gpui::Quad>>) {
    let (Some(mut incremental), Some(mut full)) = (renderer(true), renderer(false)) else {
        return;
    };

    for (index, quads) in frames.into_iter().enumerate() {
        for renderer in [&mut incremental, &mut full] {
            renderer.begin_frame();
            renderer
                .draw(&scene_of(quads.clone()))
                .expect("the frame draws");
        }
        let restricted = incremental.read_composed();
        let complete = full.read_composed();
        let worst = restricted.max_difference(&complete);
        assert!(
            worst <= 1,
            "frame {index} differed by {worst} between a restricted and a full redraw"
        );
    }
}

#[test]
fn recolouring_in_place_leaves_nothing_stale() {
    let at = bounds(16.0, 16.0, 32.0, 32.0);
    agrees_frame_by_frame(vec![
        vec![quad(at, rgb(1.0, 0.0, 0.0))],
        vec![quad(at, rgb(0.0, 1.0, 0.0))],
        vec![quad(at, rgb(0.0, 0.0, 1.0))],
    ]);
}

#[test]
fn a_quad_that_appears_and_disappears_leaves_nothing_stale() {
    let background = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.1, 0.1, 0.1));
    let popover = quad(bounds(8.0, 8.0, 20.0, 20.0), rgb(1.0, 1.0, 0.0));
    agrees_frame_by_frame(vec![
        vec![background],
        vec![background, popover],
        // The disappearance is the case that catches damage derived only from the new frame:
        // nothing in frame three mentions the region the popover used to occupy.
        vec![background],
    ]);
}

#[test]
fn a_quad_that_moves_leaves_nothing_behind() {
    let background = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.1, 0.1, 0.1));
    let mut frames = Vec::new();
    for step in 0..6 {
        let moving = quad(
            bounds(4.0 + step as f32 * 8.0, 24.0, 16.0, 16.0),
            rgb(1.0, 0.0, 1.0),
        );
        frames.push(vec![background, moving]);
    }
    agrees_frame_by_frame(frames);
}

#[test]
fn two_overlapping_quads_swapping_order_are_noticed() {
    // The case the draw order is kept in the hash for. Both quads exist in both frames with
    // identical geometry and colour; only which one is on top changes. A comparison blind to
    // order would damage nothing and leave the wrong quad showing.
    let lower = quad(bounds(8.0, 8.0, 32.0, 32.0), rgb(1.0, 0.0, 0.0));
    let upper = quad(bounds(24.0, 24.0, 32.0, 32.0), rgb(0.0, 0.0, 1.0));
    agrees_frame_by_frame(vec![vec![lower, upper], vec![upper, lower]]);
}

#[test]
fn a_quad_growing_over_its_neighbours_leaves_nothing_stale() {
    let background = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.1, 0.1, 0.1));
    let mut frames = Vec::new();
    for step in 1..6 {
        frames.push(vec![
            background,
            quad(bounds(8.0, 8.0, step as f32 * 10.0, 40.0), rgb(0.0, 1.0, 1.0)),
        ]);
    }
    agrees_frame_by_frame(frames);
}

#[test]
fn a_scene_that_does_not_change_at_all_still_matches() {
    // The frame where damage is legitimately empty. zgui skips it entirely and keeps the composed
    // target, so what this proves is that the kept target is still right.
    let quads = vec![
        quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.2, 0.2, 0.2)),
        quad(bounds(16.0, 16.0, 32.0, 32.0), rgb(0.9, 0.4, 0.1)),
    ];
    agrees_frame_by_frame(vec![quads.clone(), quads.clone(), quads]);
}

#[test]
fn damage_actually_restricts_the_redraw() {
    // The rest of the file would pass just as well if damage always went full, so this is what
    // says the mechanism is doing anything: a small change must produce a small redraw.
    let Some(mut renderer) = renderer(true) else {
        return;
    };
    let background = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.1, 0.1, 0.1));
    let spot = bounds(28.0, 28.0, 8.0, 8.0);

    renderer.begin_frame();
    renderer
        .draw(&scene_of(vec![background, quad(spot, rgb(1.0, 0.0, 0.0))]))
        .expect("the first frame draws");

    renderer.begin_frame();
    let outcome = renderer
        .draw(&scene_of(vec![background, quad(spot, rgb(0.0, 1.0, 0.0))]))
        .expect("the second frame draws");

    let zgui_render::FrameOutcome::Presented(stats) = outcome else {
        panic!("the second frame should have been presented, was {outcome:?}");
    };
    let surface = (SIDE * SIDE) as u64;
    assert!(
        stats.damage_px < surface / 4,
        "recolouring an 8x8 spot redrew {} of {surface} pixels",
        stats.damage_px
    );
}

#[test]
fn an_unchanged_frame_is_skipped_without_being_translated() {
    // The cheapest frame is the one that is never built. A window redrawn while nothing about it
    // changed — an idle repaint, a compositor asking again — should cost a comparison and nothing
    // else, so this asserts the frame is reported as undamaged rather than presented.
    let Some(mut renderer) = renderer(true) else {
        return;
    };
    let quads = vec![
        quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.2, 0.2, 0.2)),
        quad(bounds(16.0, 16.0, 32.0, 32.0), rgb(0.9, 0.4, 0.1)),
    ];

    renderer.begin_frame();
    renderer
        .draw(&scene_of(quads.clone()))
        .expect("the first frame draws");

    renderer.begin_frame();
    let outcome = renderer
        .draw(&scene_of(quads))
        .expect("the second frame is considered");

    assert!(
        matches!(
            outcome,
            zgui_render::FrameOutcome::Skipped(zgui_render::SkipReason::Undamaged)
        ),
        "an identical frame should be skipped, was {outcome:?}"
    );
}
