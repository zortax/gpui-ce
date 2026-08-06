//! Turning gpui's geometry and colour vocabulary into zgui's.
//!
//! The two libraries agree about far more than they disagree about — both measure the frame in
//! device pixels with y growing downward, and both put the origin at the top left — so almost
//! everything here is a rename. The two places it is not are worth knowing about:
//!
//! - gpui's corner radii are circular (one scalar per corner); zgui's are elliptical (a horizontal
//!   and a vertical radius per corner), because that is what `border-radius` specifies. Widening a
//!   circle to an ellipse is lossless, so this direction is safe; the reverse would not be.
//! - gpui's colours are HSLA with hue in turns; zgui's carry their colour space with them. Both
//!   are gamma-encoded sRGB underneath, so the conversion runs through gpui's own
//!   [`Rgba`] and asserts nothing about light.

use gpui::{Bounds, Corners, DevicePixels, Edges, Hsla, Point, Rgba, ScaledPixels, Size};
use zgui_color::Color;
use zgui_geom::{Device, DevicePx, Rect, Vec2};

/// A gpui point in scaled pixels, as a zgui device-space point.
pub fn point(point: Point<ScaledPixels>) -> zgui_geom::Point<DevicePx, Device> {
    zgui_geom::Point::new(DevicePx(point.x.0), DevicePx(point.y.0))
}

/// A gpui size in scaled pixels, as a zgui device-space size.
pub fn size(size: Size<ScaledPixels>) -> zgui_geom::Size<DevicePx, Device> {
    zgui_geom::Size::new(DevicePx(size.width.0), DevicePx(size.height.0))
}

/// A gpui bounds in scaled pixels, as a zgui device-space rectangle.
pub fn rect(bounds: Bounds<ScaledPixels>) -> Rect<DevicePx, Device> {
    Rect::new(point(bounds.origin), size(bounds.size))
}

/// A gpui bounds in whole device pixels, as a zgui integer device-space rectangle.
///
/// Used for atlas tiles, which are addressed in texels rather than in fractional pixels.
pub fn texel_rect(bounds: Bounds<DevicePixels>) -> Rect<i32, Device> {
    Rect::new(
        zgui_geom::Point::new(bounds.origin.x.0, bounds.origin.y.0),
        zgui_geom::Size::new(bounds.size.width.0, bounds.size.height.0),
    )
}

/// A gpui size in whole device pixels, as a zgui integer device-space size.
pub fn texel_size(size: Size<DevicePixels>) -> zgui_geom::Size<i32, Device> {
    zgui_geom::Size::new(size.width.0, size.height.0)
}

/// A zgui integer device-space rectangle, as a gpui bounds in whole device pixels.
pub fn device_bounds(rect: Rect<i32, Device>) -> Bounds<DevicePixels> {
    Bounds {
        origin: Point {
            x: DevicePixels(rect.origin.x),
            y: DevicePixels(rect.origin.y),
        },
        size: Size {
            width: DevicePixels(rect.size.width),
            height: DevicePixels(rect.size.height),
        },
    }
}

/// A gpui atlas tile, as the zgui tile a sprite samples.
///
/// The two records carry the same three facts; only the spelling differs.
pub fn atlas_tile(tile: gpui::AtlasTile) -> zgui_atlas::AtlasTile {
    zgui_atlas::AtlasTile {
        texture: zgui_atlas::TextureId::new(
            match tile.texture_id.kind {
                gpui::AtlasTextureKind::Monochrome => zgui_atlas::TextureKind::Mono,
                gpui::AtlasTextureKind::Polychrome => zgui_atlas::TextureKind::Color,
                gpui::AtlasTextureKind::Subpixel => zgui_atlas::TextureKind::Subpixel,
            },
            tile.texture_id.index,
        ),
        tile: zgui_atlas::TileId(tile.tile_id.0),
        bounds: texel_rect(tile.bounds),
    }
}

/// A gpui colour, as a zgui gamma-encoded sRGB colour.
pub fn color(color: Hsla) -> Color {
    let Rgba { r, g, b, a } = Rgba::from(color);
    Color::srgb(r, g, b, a)
}

/// gpui's circular corner radii, as zgui's elliptical ones.
///
/// Widening a circle to an ellipse is lossless; the reverse would not be.
pub fn corner_radii(radii: Corners<ScaledPixels>) -> zgui_geom::Corners<Vec2<DevicePx>> {
    let widen = |radius: ScaledPixels| Vec2::new(DevicePx(radius.0), DevicePx(radius.0));
    zgui_geom::Corners::new(
        widen(radii.top_left),
        widen(radii.top_right),
        widen(radii.bottom_right),
        widen(radii.bottom_left),
    )
}

/// gpui's circular corner radii, flattened into the `[f32; 8]` an instance carries.
pub fn corner_radii_array(radii: Corners<ScaledPixels>) -> [f32; 8] {
    [
        radii.top_left.0,
        radii.top_left.0,
        radii.top_right.0,
        radii.top_right.0,
        radii.bottom_right.0,
        radii.bottom_right.0,
        radii.bottom_left.0,
        radii.bottom_left.0,
    ]
}

/// gpui's border widths, in the `[top, right, bottom, left]` order an instance carries.
pub fn border_widths(edges: Edges<ScaledPixels>) -> [f32; 4] {
    [edges.top.0, edges.right.0, edges.bottom.0, edges.left.0]
}

/// A gpui bounds, flattened into the `[x, y, width, height]` an instance carries.
pub fn bounds_array(bounds: Bounds<ScaledPixels>) -> [f32; 4] {
    [
        bounds.origin.x.0,
        bounds.origin.y.0,
        bounds.size.width.0,
        bounds.size.height.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_red_survives_the_round_trip() {
        let red = Hsla {
            h: 0.0,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        };
        let converted = color(red);
        assert_eq!(converted.space(), zgui_color::ColorSpace::Srgb);
        assert_eq!(converted.alpha(), 1.0);
        let [r, g, b] = converted.components();
        assert!(r > 0.99, "red channel was {r}");
        assert!(g < 0.01, "green channel was {g}");
        assert!(b < 0.01, "blue channel was {b}");
    }

    #[test]
    fn transparency_is_carried_rather_than_folded_into_the_channels() {
        let half = Hsla {
            h: 0.0,
            s: 1.0,
            l: 0.5,
            a: 0.5,
        };
        let converted = color(half);
        assert_eq!(converted.alpha(), 0.5);
        // Not premultiplied here: `Color` is a straight-alpha value, and the renderer
        // premultiplies on the way to the GPU.
        assert!(converted.components()[0] > 0.99);
    }

    #[test]
    fn a_circular_radius_widens_to_a_square_ellipse() {
        let radii = Corners {
            top_left: ScaledPixels(4.0),
            top_right: ScaledPixels(8.0),
            bottom_right: ScaledPixels(0.0),
            bottom_left: ScaledPixels(2.0),
        };
        assert_eq!(
            corner_radii_array(radii),
            [4.0, 4.0, 8.0, 8.0, 0.0, 0.0, 2.0, 2.0]
        );
    }

    #[test]
    fn bounds_keep_their_origin_and_extent() {
        let bounds = Bounds {
            origin: Point {
                x: ScaledPixels(3.0),
                y: ScaledPixels(5.0),
            },
            size: Size {
                width: ScaledPixels(20.0),
                height: ScaledPixels(10.0),
            },
        };
        assert_eq!(bounds_array(bounds), [3.0, 5.0, 20.0, 10.0]);
        let converted = rect(bounds);
        assert_eq!(converted.origin.x, DevicePx(3.0));
        assert_eq!(converted.size.height, DevicePx(10.0));
    }
}
