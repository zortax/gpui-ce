//! Paths, as cached coverage masks.
//!
//! gpui hands a path over already tessellated: a list of triangles, each vertex carrying an `st`
//! coordinate that encodes a quadratic curve by the Loop–Blinn rule, `s² − t ≤ 0`. The source
//! outline is gone by then. zgui's vector passes want an outline, so there is nothing to hand
//! them — but a triangle mesh can be rasterised into an alpha mask, and a mask is exactly what a
//! [`MonoSprite`](zgui_scene::MonoSprite) draws.
//!
//! That turns out to be a good fit rather than a workaround. The mask is keyed by the mesh's own
//! content, so the icons, dividers and custom chrome that paths are actually used for are
//! rasterised once and then cost no more than a glyph. The mask is also measured from the path's
//! own origin, so a path that only moves reuses it.
//!
//! Two things are genuinely lost, both recorded in `LIMITATIONS.md`: a path whose fill is a
//! gradient is drawn in the ramp's mean colour, because a mono sprite carries one colour and zgui
//! will not apply a raster mask to a direct draw; and a path that changes shape every frame is
//! re-rasterised on the CPU rather than on the GPU.

use gpui::{Background, BackgroundKind, Bounds, Hsla, Path, Rgba, ScaledPixels};
use zgui_atlas::{Atlas, AtlasKey, AtlasTile, TextureKind};

/// The largest mask this will rasterise, per side, in device pixels.
///
/// A path larger than this is skipped and counted. The cap exists because rasterising happens on
/// the CPU: a full-screen path would cost several milliseconds per frame, which is worse than the
/// missing drawing it replaces.
const MAX_SIDE: i32 = 2048;

/// Samples per axis inside each pixel.
///
/// Four samples per pixel is where a diagonal edge stops looking stepped at ordinary sizes.
/// gpui's own renderer antialiases paths analytically from the curve gradient and with
/// multisampling, so this is close but not identical.
const SAMPLES: i32 = 2;

/// A rasterised path: where its mask landed, and where on the surface it goes.
pub struct Mask {
    /// The coverage tile.
    pub tile: AtlasTile,
    /// The rectangle the tile covers on the surface.
    pub bounds: Bounds<ScaledPixels>,
    /// The colour the coverage is tinted with.
    pub color: Hsla,
}

/// The mask for `path`, rasterising it on first use.
///
/// `None` when the path covers nothing, or is too large to rasterise.
pub fn mask(atlas: &mut Atlas, path: &Path<ScaledPixels>) -> Option<Mask> {
    let painted = path.bounds.intersect(&path.content_mask.bounds);
    // Whole pixels, rounded outwards: a path ending halfway through a pixel still tints it.
    let left = painted.origin.x.0.floor();
    let top = painted.origin.y.0.floor();
    let right = (painted.origin.x.0 + painted.size.width.0).ceil();
    let bottom = (painted.origin.y.0 + painted.size.height.0).ceil();
    let width = (right - left) as i32;
    let height = (bottom - top) as i32;

    if width <= 0 || height <= 0 {
        return None;
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }

    let color = flat_color(&path.color)?;
    let handle = handle_of(path, left, top);
    let key = AtlasKey::new(handle, TextureKind::Mono);
    let size = zgui_geom::Size::new(width, height);

    let tile = atlas
        .get_or_insert(key, size, || raster(path, left, top, width, height))
        .inspect_err(|error| log::warn!("gpui_zgui: could not cache a path mask: {error}"))
        .ok()?;

    Some(Mask {
        tile,
        bounds: Bounds {
            origin: gpui::Point {
                x: ScaledPixels(left),
                y: ScaledPixels(top),
            },
            size: gpui::Size {
                width: ScaledPixels(width as f32),
                height: ScaledPixels(height as f32),
            },
        },
        color,
    })
}

/// The one colour a coverage mask can be tinted with.
///
/// A gradient collapses to the mean of its stops. That is the same answer zgui gives in
/// `Paint::flat_color`, and for the same stated reason: a shape drawn in roughly the right colour
/// beats a shape that is not drawn at all.
fn flat_color(background: &Background) -> Option<Hsla> {
    match background.kind() {
        BackgroundKind::Solid(color) => Some(color),
        BackgroundKind::PatternSlash { color, .. } | BackgroundKind::Checkerboard { color, .. } => {
            Some(color)
        }
        BackgroundKind::LinearGradient { stops, .. } => {
            let mean = |pick: fn(&Rgba) -> f32| {
                stops
                    .iter()
                    .map(|stop| pick(&Rgba::from(stop.color)))
                    .sum::<f32>()
                    / stops.len() as f32
            };
            Some(Hsla::from(Rgba {
                r: mean(|c| c.r),
                g: mean(|c| c.g),
                b: mean(|c| c.b),
                a: mean(|c| c.a),
            }))
        }
    }
}

/// A stable identity for a path's mask.
///
/// Measured relative to the mask's own origin, so a path that is only translated keeps its
/// identity and reuses the raster it already has — which is what makes a moving path cheap.
fn handle_of(path: &Path<ScaledPixels>, left: f32, top: f32) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = collections::FxHasher::default();
    "gpui_zgui::path".hash(&mut hasher);
    for vertex in &path.vertices {
        (vertex.xy_position.x.0 - left).to_bits().hash(&mut hasher);
        (vertex.xy_position.y.0 - top).to_bits().hash(&mut hasher);
        vertex.st_position.x.to_bits().hash(&mut hasher);
        vertex.st_position.y.to_bits().hash(&mut hasher);
    }
    // The mask is coverage only, but the clip decides which of it survives.
    for value in [
        path.content_mask.bounds.origin.x.0 - left,
        path.content_mask.bounds.origin.y.0 - top,
        path.content_mask.bounds.size.width.0,
        path.content_mask.bounds.size.height.0,
    ] {
        value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Rasterises the triangle mesh into single-channel coverage.
fn raster(path: &Path<ScaledPixels>, left: f32, top: f32, width: i32, height: i32) -> Vec<u8> {
    let mut coverage = vec![0f32; (width * height) as usize];
    let step = 1.0 / SAMPLES as f32;
    let weight = 1.0 / (SAMPLES * SAMPLES) as f32;

    for triangle in path.vertices.chunks_exact(3) {
        let xs = [
            triangle[0].xy_position.x.0 - left,
            triangle[1].xy_position.x.0 - left,
            triangle[2].xy_position.x.0 - left,
        ];
        let ys = [
            triangle[0].xy_position.y.0 - top,
            triangle[1].xy_position.y.0 - top,
            triangle[2].xy_position.y.0 - top,
        ];

        // Twice the signed area. A degenerate triangle covers nothing and would divide by zero.
        let area = (xs[1] - xs[0]) * (ys[2] - ys[0]) - (xs[2] - xs[0]) * (ys[1] - ys[0]);
        if area.abs() < f32::EPSILON {
            continue;
        }

        let min_x = xs.iter().copied().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
        let max_x = (xs.iter().copied().fold(f32::MIN, f32::max).ceil() as i32).min(width);
        let min_y = ys.iter().copied().fold(f32::MAX, f32::min).floor().max(0.0) as i32;
        let max_y = (ys.iter().copied().fold(f32::MIN, f32::max).ceil() as i32).min(height);

        for row in min_y..max_y {
            for column in min_x..max_x {
                let mut hits = 0.0;
                for sub_y in 0..SAMPLES {
                    for sub_x in 0..SAMPLES {
                        let x = column as f32 + (sub_x as f32 + 0.5) * step;
                        let y = row as f32 + (sub_y as f32 + 0.5) * step;

                        // Barycentric coordinates, which serve twice: they say whether the sample
                        // is inside the triangle, and they interpolate `st` to it.
                        let w0 = ((xs[1] - x) * (ys[2] - y) - (xs[2] - x) * (ys[1] - y)) / area;
                        let w1 = ((xs[2] - x) * (ys[0] - y) - (xs[0] - x) * (ys[2] - y)) / area;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }

                        let s = w0 * triangle[0].st_position.x
                            + w1 * triangle[1].st_position.x
                            + w2 * triangle[2].st_position.x;
                        let t = w0 * triangle[0].st_position.y
                            + w1 * triangle[1].st_position.y
                            + w2 * triangle[2].st_position.y;
                        // The Loop–Blinn test. An interior triangle carries `st = (0, 1)` at every
                        // vertex, so this is always true for it and the curve triangles are the
                        // only ones it actually decides.
                        if s * s - t <= 0.0 {
                            hits += weight;
                        }
                    }
                }
                if hits > 0.0 {
                    let at = (row * width + column) as usize;
                    // Saturating rather than replacing: a tessellation's interior triangles meet
                    // edge to edge, and a curve triangle overlaps the interior it completes.
                    coverage[at] = (coverage[at] + hits).min(1.0);
                }
            }
        }
    }

    coverage
        .into_iter()
        .map(|value| (value * 255.0).round().clamp(0.0, 255.0) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, px};

    /// A filled square, as `Path::line_to` would tessellate one.
    fn square(size: f32) -> Path<ScaledPixels> {
        let mut path = Path::new(Point {
            x: px(0.0),
            y: px(0.0),
        });
        path.line_to(Point {
            x: px(size),
            y: px(0.0),
        });
        path.line_to(Point {
            x: px(size),
            y: px(size),
        });
        path.line_to(Point {
            x: px(0.0),
            y: px(size),
        });
        let mut path = path.scale(1.0);
        path.content_mask = gpui::ContentMask {
            bounds: Bounds {
                origin: Point {
                    x: ScaledPixels(0.0),
                    y: ScaledPixels(0.0),
                },
                size: gpui::Size {
                    width: ScaledPixels(64.0),
                    height: ScaledPixels(64.0),
                },
            },
        };
        path
    }

    #[test]
    fn a_filled_square_covers_its_interior() {
        let path = square(16.0);
        let bytes = raster(&path, 0.0, 0.0, 16, 16);
        // Well inside the shape, coverage is complete.
        assert_eq!(bytes[(8 * 16 + 8) as usize], 255);
    }

    #[test]
    fn nothing_is_covered_outside_the_shape() {
        let mut path = square(8.0);
        path.content_mask.bounds.size = gpui::Size {
            width: ScaledPixels(16.0),
            height: ScaledPixels(16.0),
        };
        let bytes = raster(&path, 0.0, 0.0, 16, 16);
        assert_eq!(
            bytes[(12 * 16 + 12) as usize],
            0,
            "a pixel beyond the square should be clear"
        );
    }

    #[test]
    fn a_translated_path_keeps_its_identity() {
        // The mask is measured from its own origin, so moving a path must not re-rasterise it.
        let here = square(16.0);
        let mut moved = square(16.0);
        for vertex in &mut moved.vertices {
            vertex.xy_position.x = ScaledPixels(vertex.xy_position.x.0 + 40.0);
        }
        moved.content_mask.bounds.origin.x = ScaledPixels(40.0);
        assert_eq!(handle_of(&here, 0.0, 0.0), handle_of(&moved, 40.0, 0.0));
    }

    #[test]
    fn a_differently_shaped_path_gets_a_different_identity() {
        assert_ne!(
            handle_of(&square(16.0), 0.0, 0.0),
            handle_of(&square(20.0), 0.0, 0.0)
        );
    }

    #[test]
    fn a_gradient_fill_collapses_to_the_mean_of_its_stops() {
        let background = gpui::linear_gradient(
            0.0,
            gpui::linear_color_stop(Hsla::from(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }), 0.0),
            gpui::linear_color_stop(Hsla::from(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }), 1.0),
        );
        let mean = Rgba::from(flat_color(&background).expect("a ramp has a mean"));
        assert!((mean.r - 0.5).abs() < 0.02, "red was {}", mean.r);
        assert!((mean.b - 0.5).abs() < 0.02, "blue was {}", mean.b);
    }
}
