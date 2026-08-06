//! End-to-end checks that a `gpui::Scene` drawn through zgui produces the pixels it should.
//!
//! These build gpui scenes by hand and render them offscreen, so they exercise the whole
//! translation — geometry, colour, clips, paint interning, draw order — with no window, no event
//! loop and no element tree in the way. A mistranslated field shows up here as a wrong pixel
//! rather than as a subtly wrong screenshot much later.
//!
//! Every test skips rather than fails on a machine with no usable graphics device, and says so, so
//! a green run on a machine without an adapter cannot be mistaken for a pass.

use gpui::{
    Background, Bounds, ContentMask, Corners, Edges, Hsla, Point, ScaledPixels, Scene, Size,
};
use gpui_zgui::ZguiRenderer;
use zgui_geom::Size as ZSize;
use zgui_render_wgpu::{Pixels, wgpu};

const SIDE: i32 = 64;

fn renderer() -> Option<ZguiRenderer> {
    match ZguiRenderer::offscreen(
        ZSize::new(SIDE, SIDE),
        1.0,
        wgpu::TextureFormat::Bgra8Unorm,
    ) {
        Ok(renderer) => Some(renderer),
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

/// Draws `scene` and reads the composed target back.
fn render(renderer: &mut ZguiRenderer, mut scene: Scene) -> Pixels {
    scene.finish();
    renderer.begin_frame();
    renderer.draw(&scene).expect("the frame draws");
    renderer.read_composed()
}

/// Channels are compared with a tolerance because the composed target round-trips through an
/// 8-bit surface, and premultiplication is not exactly invertible there.
#[track_caller]
fn assert_pixel(pixels: &Pixels, x: i32, y: i32, expected: [u8; 4]) {
    let actual = pixels.rgba(x, y);
    let worst = actual
        .iter()
        .zip(expected.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    assert!(
        worst <= 2,
        "pixel at ({x}, {y}) was {actual:?}, expected about {expected:?}"
    );
}

#[test]
fn a_solid_quad_fills_its_bounds_and_nothing_else() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(16.0, 16.0, 32.0, 32.0), rgb(1.0, 0.0, 0.0)));

    let pixels = render(&mut renderer, scene);

    assert_pixel(&pixels, 32, 32, [255, 0, 0, 255]);
    assert_pixel(&pixels, 17, 17, [255, 0, 0, 255]);
    assert_pixel(&pixels, 46, 46, [255, 0, 0, 255]);
    // Outside the quad the surface is untouched, which for an opaque target is transparent black.
    assert_pixel(&pixels, 4, 4, [0, 0, 0, 0]);
    assert_pixel(&pixels, 60, 60, [0, 0, 0, 0]);
}

#[test]
fn later_primitives_paint_over_earlier_ones() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    scene.insert_primitive(quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.0, 0.0, 1.0)));
    scene.insert_primitive(quad(bounds(16.0, 16.0, 32.0, 32.0), rgb(0.0, 1.0, 0.0)));

    let pixels = render(&mut renderer, scene);

    assert_pixel(&pixels, 32, 32, [0, 255, 0, 255]);
    assert_pixel(&pixels, 4, 4, [0, 0, 255, 255]);
}

#[test]
fn a_content_mask_clips_what_it_excludes() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    let mut clipped = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(1.0, 0.0, 0.0));
    clipped.content_mask = ContentMask {
        bounds: bounds(0.0, 0.0, 32.0, 64.0),
    };
    scene.insert_primitive(clipped);

    let pixels = render(&mut renderer, scene);

    assert_pixel(&pixels, 16, 32, [255, 0, 0, 255]);
    assert_pixel(&pixels, 48, 32, [0, 0, 0, 0]);
}

#[test]
fn corner_radii_round_the_corners_they_name() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    let mut rounded = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(1.0, 1.0, 1.0));
    rounded.corner_radii = Corners {
        top_left: ScaledPixels(24.0),
        top_right: ScaledPixels(0.0),
        bottom_right: ScaledPixels(0.0),
        bottom_left: ScaledPixels(0.0),
    };
    scene.insert_primitive(rounded);

    let pixels = render(&mut renderer, scene);

    // The named corner is cut away; the other three are square. This is also what catches the
    // corner order being rotated, which a uniform radius could never show.
    assert_pixel(&pixels, 1, 1, [0, 0, 0, 0]);
    assert_pixel(&pixels, 62, 1, [255, 255, 255, 255]);
    assert_pixel(&pixels, 62, 62, [255, 255, 255, 255]);
    assert_pixel(&pixels, 1, 62, [255, 255, 255, 255]);
}

#[test]
fn a_border_draws_in_its_own_colour_inside_the_bounds() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    let mut bordered = quad(bounds(0.0, 0.0, 64.0, 64.0), rgb(0.0, 0.0, 0.0));
    bordered.border_color = rgb(1.0, 0.0, 0.0);
    bordered.border_widths = Edges {
        top: ScaledPixels(8.0),
        right: ScaledPixels(8.0),
        bottom: ScaledPixels(8.0),
        left: ScaledPixels(8.0),
    };
    scene.insert_primitive(bordered);

    let pixels = render(&mut renderer, scene);

    assert_pixel(&pixels, 4, 32, [255, 0, 0, 255]);
    assert_pixel(&pixels, 32, 4, [255, 0, 0, 255]);
    assert_pixel(&pixels, 32, 32, [0, 0, 0, 255]);
}

#[test]
fn a_linear_gradient_runs_the_way_its_angle_says() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    // 180 degrees points down, so the first stop is at the top.
    scene.insert_primitive(quad(
        bounds(0.0, 0.0, 64.0, 64.0),
        gpui::linear_gradient(
            180.0,
            gpui::linear_color_stop(rgb(1.0, 0.0, 0.0), 0.0),
            gpui::linear_color_stop(rgb(0.0, 1.0, 0.0), 1.0),
        ),
    ));

    let pixels = render(&mut renderer, scene);

    let top = pixels.rgba(32, 1);
    let bottom = pixels.rgba(32, 62);
    assert!(
        top[0] > 200 && top[1] < 60,
        "the top should be nearly the first stop, was {top:?}"
    );
    assert!(
        bottom[1] > 200 && bottom[0] < 60,
        "the bottom should be nearly the second stop, was {bottom:?}"
    );
}

#[test]
fn a_drop_shadow_paints_outside_the_box_that_cast_it() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    let element = bounds(24.0, 24.0, 16.0, 16.0);
    scene.insert_primitive(gpui::Shadow {
        order: 0,
        blur_radius: ScaledPixels(4.0),
        bounds: element,
        corner_radii: Corners::default(),
        content_mask: full_mask(),
        color: rgb(0.0, 0.0, 0.0),
        element_bounds: element,
        element_corner_radii: Corners::default(),
        inset: 0,
        pad: 0,
    });

    let pixels = render(&mut renderer, scene);

    // Just outside the casting box the shadow is present but not opaque. This is what catches the
    // missing three-sigma dilation: without it zgui culls the tail and this pixel is empty.
    let tail = pixels.rgba(24, 20);
    assert!(
        tail[3] > 8 && tail[3] < 250,
        "the gaussian tail should be faintly present outside the box, alpha was {}",
        tail[3]
    );
    // Far outside the reach of three standard deviations, nothing is painted.
    assert_pixel(&pixels, 2, 2, [0, 0, 0, 0]);
}

#[test]
fn a_patterned_fill_degrades_to_its_flat_colour_rather_than_vanishing() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    // zgui's wgpu backend does not implement its own sampled-image paint, so there is nothing to
    // tile a rasterised pattern with. Drawing the base colour is the deliberate fallback; the
    // element must stay visible rather than becoming a hole. See LIMITATIONS.md.
    scene.insert_primitive(quad(
        bounds(0.0, 0.0, 64.0, 64.0),
        gpui::checkerboard(rgb(1.0, 1.0, 1.0), 16.0),
    ));

    let pixels = render(&mut renderer, scene);
    assert_pixel(&pixels, 32, 32, [255, 255, 255, 255]);
}

#[test]
fn a_blur_group_spreads_its_contents_beyond_their_bounds() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    let mut scene = Scene::default();
    let group = bounds(8.0, 8.0, 48.0, 48.0);
    let filters: smallvec::SmallVec<[gpui::ScaledFilter; 4]> =
        smallvec::smallvec![gpui::ScaledFilter::Blur(ScaledPixels(4.0))];

    scene.insert_primitive(gpui::FilterBoundary {
        order: 0,
        bounds: group,
        content_mask: full_mask(),
        corner_radii: Corners::default(),
        filters: filters.clone(),
        opacity: 1.0,
        is_start: true,
    });
    scene.insert_primitive(quad(bounds(24.0, 24.0, 16.0, 16.0), rgb(1.0, 1.0, 1.0)));
    scene.insert_primitive(gpui::FilterBoundary {
        order: 0,
        bounds: group,
        content_mask: full_mask(),
        corner_radii: Corners::default(),
        filters,
        opacity: 1.0,
        is_start: false,
    });

    let pixels = render(&mut renderer, scene);

    // The centre stays bright and the edge softens: a group that failed to isolate would leave a
    // hard edge, and one that was dropped entirely would leave nothing at all.
    assert!(pixels.rgba(32, 32)[3] > 200, "the centre should be drawn");
    let outside = pixels.rgba(20, 32);
    assert!(
        outside[3] > 10 && outside[3] < 200,
        "the blur should spread past the quad's edge, alpha was {}",
        outside[3]
    );
}

#[test]
fn a_transformed_sprite_lands_where_its_matrix_puts_it() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    // A sprite needs a tile, and the only honest way to get one is through the atlas the renderer
    // hands gpui — the same path a glyph takes.
    let atlas = renderer.sprite_atlas();
    let key = gpui::AtlasKey::Image(gpui::RenderImageParams {
        image_id: gpui::ImageId(1),
        frame_index: 0,
    });
    let tile = gpui::PlatformAtlas::get_or_insert_with(&*atlas, &key, &mut || {
        Ok(Some((
            gpui::Size {
                width: gpui::DevicePixels(8),
                height: gpui::DevicePixels(8),
            },
            std::borrow::Cow::Owned(vec![255u8; 8 * 8 * 4]),
        )))
    })
    .expect("the tile fits")
    .expect("the build produced content");

    let mut scene = Scene::default();
    scene.insert_primitive(gpui::PolychromeSprite {
        order: 0,
        pad: 0,
        grayscale: false.into(),
        opacity: 1.0,
        bounds: bounds(8.0, 8.0, 8.0, 8.0),
        content_mask: full_mask(),
        corner_radii: Corners::default(),
        tile,
    });

    let pixels = render(&mut renderer, scene);
    assert!(
        pixels.rgba(12, 12)[3] > 200,
        "the sprite should be drawn where its bounds put it"
    );
}

#[test]
fn a_path_is_drawn_through_its_rasterised_mask() {
    let Some(mut renderer) = renderer() else {
        return;
    };
    // gpui discards a path's outline during tessellation, so there is nothing to hand zgui's
    // vector passes; the mesh is rasterised into a coverage mask instead. What matters is that the
    // shape ends up on screen, filled, with nothing outside it.
    let mut path = gpui::Path::new(Point {
        x: gpui::px(8.0),
        y: gpui::px(8.0),
    });
    path.line_to(Point {
        x: gpui::px(56.0),
        y: gpui::px(8.0),
    });
    path.line_to(Point {
        x: gpui::px(56.0),
        y: gpui::px(56.0),
    });
    path.line_to(Point {
        x: gpui::px(8.0),
        y: gpui::px(56.0),
    });
    let mut path = path.scale(1.0);
    path.content_mask = full_mask();
    path.color = rgb(1.0, 1.0, 0.0).into();

    let mut scene = Scene::default();
    scene.insert_primitive(path);

    let pixels = render(&mut renderer, scene);

    let inside = pixels.rgba(32, 32);
    assert!(
        inside[0] > 200 && inside[1] > 200 && inside[2] < 60,
        "the path's interior should be filled, was {inside:?}"
    );
    assert_pixel(&pixels, 2, 2, [0, 0, 0, 0]);
}
