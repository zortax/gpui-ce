//! Rewriting a [`gpui::Scene`] as a [`zgui_scene::Scene`].
//!
//! The two display lists are close cousins: both are structure-of-arrays over `#[repr(C)]`
//! instances, both allocate draw order from a bounds tree with the same "equal order implies no
//! overlap" invariant, and both carry a clip and a paint per primitive. So most of this file is a
//! field-by-field rename, and the interesting parts are the handful of places where the two
//! genuinely disagree:
//!
//! - **Draw order is not copied.** gpui has already allocated an order for every primitive, and
//!   [`gpui::Scene::batches`] yields them sorted by it. Pushing them into zgui in that sequence
//!   lets zgui allocate its *own* orders from its own bounds tree, which is what its replay and
//!   damage machinery expect. Relative order is preserved because the input sequence is a valid
//!   painting order; the numbers are not, and must not be.
//! - **Shadow extent.** gpui stores the shadow *shape* in `bounds` and dilates it by three
//!   standard deviations in the vertex shader. zgui stores the dilated extent, because that is
//!   what its culling and ordering read. The dilation therefore has to happen here.
//! - **Clips.** gpui gives every primitive a flat rectangle; zgui wants an id into a clip table.
//!   The table interns by content, so the thousands of primitives sharing a scroll container's
//!   mask collapse onto one id without a cache of our own.
//! - **Backgrounds.** gpui's gradient is not the CSS one — it normalises the direction by the
//!   quad's aspect ratio and remaps `t` through the stop percentages. [`linear_gradient`] derives
//!   the endpoints that reproduce it exactly under zgui's plain projection.
//! - **Filter groups.** gpui emits a matched pair of boundary markers around an isolated subtree;
//!   zgui takes the same shape, and both treat the pair as inseparable.

use gpui::{
    Background, BackgroundKind, Bounds, ContentMask, Corners, Hsla, PrimitiveBatch, ScaledFilter,
    ScaledPixels,
};
use smallvec::SmallVec;
use zgui_color::{GradientStop, HueInterpolation};
use zgui_scene::prim::{BorderStyle, DecorationStyle};
use zgui_scene::{
    BackdropFilter, ClipId, ClipLink, ColorSprite, Decoration, ExternalQuad, ExternalTextureId,
    Filter, GradientKind, GroupBoundary, MonoSprite, Paint, PaintRef, Quad, Scene, Shadow,
    SpatialId, SubpixelSprite,
};

use crate::atlas::ZguiAtlas;
use crate::convert;
use crate::renderer::path;
use crate::renderer::spatial::Transforms;

/// Primitives a frame contained that this translation cannot yet express.
///
/// Counted rather than logged at the point of failure: a scene with four thousand paths would
/// otherwise produce four thousand identical log lines. The renderer reports the totals once, and
/// only when they change.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsupported {
    /// Tessellated [`gpui::Path`] primitives, which need an outline zgui's vector passes can take.
    pub paths: usize,
    /// Surfaces whose texture this renderer could not adopt.
    pub foreign_surfaces: usize,
    /// Slash and checkerboard fills, drawn as their flat colour.
    pub patterns: usize,
}

impl Unsupported {
    /// Whether anything at all went unexpressed.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// State the translation keeps between frames.
#[derive(Default)]
pub struct Translator {
    /// Sprite transforms, interned into the scene's spatial tree.
    transforms: Transforms,
}

/// What one frame's translation hands down to every primitive.
struct Context<'a> {
    atlas: &'a ZguiAtlas,
    missing: Unsupported,
    /// The clip interned most recently, and the mask it came from.
    ///
    /// Interning is a hash and a table probe, and the scene asks for one *per primitive* — but
    /// consecutive primitives nearly always share a content mask, because a mask is what a scroll
    /// container or a list row imposes on everything inside it. Remembering one answer turns
    /// thousands of probes a frame into a handful. The context is rebuilt every frame, so a
    /// remembered id can never outlive the table entry it names.
    last_clip: Option<([u32; 4], ClipId)>,
    /// The same, for flat fills, which repeat even more than clips do.
    last_solid: Option<([u32; 4], PaintRef)>,
}

impl Translator {
    /// Prepares for a frame. Must be called after [`Scene::begin_frame`].
    pub fn begin_frame(&mut self, into: &mut Scene) {
        self.transforms.begin_frame(into);
    }

    /// Rewrites `from` into `into`, which must already have been begun for this frame.
    ///
    /// `externals` names the texture registered for each `from.surfaces` entry, by index.
    pub fn translate(
        &mut self,
        from: &gpui::Scene,
        into: &mut Scene,
        atlas: &ZguiAtlas,
        externals: &[Option<ExternalTextureId>],
    ) -> Unsupported {
        let mut cx = Context {
            atlas,
            missing: Unsupported::default(),
            last_clip: None,
            last_solid: None,
        };

        for batch in from.batches() {
            match batch {
                PrimitiveBatch::Shadows(range) => {
                    for shadow in &from.shadows[range] {
                        push_shadow(into, shadow, &mut cx);
                    }
                }
                PrimitiveBatch::Quads(range) => {
                    for quad in &from.quads[range] {
                        push_quad(into, quad, &mut cx);
                    }
                }
                PrimitiveBatch::Underlines(range) => {
                    for underline in &from.underlines[range] {
                        push_underline(into, underline, &mut cx);
                    }
                }
                PrimitiveBatch::MonochromeSprites { range, .. } => {
                    for sprite in &from.monochrome_sprites[range] {
                        let clip = clip_for(into, &sprite.content_mask, &mut cx);
                        let space = self.transforms.id_for(into, &sprite.transformation);
                        let mut mono = MonoSprite::new(
                            convert::rect(sprite.bounds),
                            convert::atlas_tile(sprite.tile),
                            convert::color(sprite.color),
                        )
                        .clipped(clip);
                        mono.transform = space.index();
                        into.push_mono_sprite(mono);
                    }
                }
                PrimitiveBatch::SubpixelSprites { range, .. } => {
                    for sprite in &from.subpixel_sprites[range] {
                        let clip = clip_for(into, &sprite.content_mask, &mut cx);
                        let space = self.transforms.id_for(into, &sprite.transformation);
                        let mut subpixel = SubpixelSprite::new(
                            convert::rect(sprite.bounds),
                            convert::atlas_tile(sprite.tile),
                            convert::color(sprite.color),
                        )
                        .clipped(clip);
                        subpixel.transform = space.index();
                        into.push_subpixel_sprite(subpixel);
                    }
                }
                PrimitiveBatch::PolychromeSprites { range, .. } => {
                    for sprite in &from.polychrome_sprites[range] {
                        let clip = clip_for(into, &sprite.content_mask, &mut cx);
                        let mut color_sprite = ColorSprite::new(
                            convert::rect(sprite.bounds),
                            convert::atlas_tile(sprite.tile),
                        )
                        .clipped(clip);
                        color_sprite.radii = convert::corner_radii_array(sprite.corner_radii);
                        color_sprite.opacity = sprite.opacity;
                        if sprite.grayscale.get() {
                            color_sprite.flags |= ColorSprite::GRAYSCALE;
                        }
                        into.push_color_sprite(color_sprite);
                    }
                }
                PrimitiveBatch::Surfaces(range) => {
                    let start = range.start;
                    for (offset, surface) in from.surfaces[range].iter().enumerate() {
                        let texture = externals.get(start + offset).copied().flatten();
                        push_surface(into, surface, texture, &mut cx);
                    }
                }
                PrimitiveBatch::BackdropFilters(range) => {
                    for backdrop in &from.backdrop_filters[range] {
                        push_backdrop(into, backdrop, &mut cx);
                    }
                }
                PrimitiveBatch::FilterBoundary(index) => {
                    if let Some(boundary) = from.filter_boundaries.get(index) {
                        push_group(into, boundary, &mut cx);
                    }
                }
                PrimitiveBatch::Paths(range) => {
                    for path in &from.paths[range] {
                        push_path(into, path, &mut cx);
                    }
                }
            }
        }

        cx.missing
    }
}

/// A path, as a cached coverage mask.
///
/// gpui tessellates a path into triangles and discards the outline, so there is nothing to hand
/// zgui's vector passes. Rasterising the mesh into an alpha mask and drawing it as a mono sprite
/// costs one raster per distinct shape, which the atlas then caches — see [`path`] for what that
/// gives up.
fn push_path(into: &mut Scene, path: &gpui::Path<ScaledPixels>, cx: &mut Context<'_>) {
    let Some(mask) = cx.atlas.with_atlas(|atlas| path::mask(atlas, path)) else {
        cx.missing.paths += 1;
        return;
    };
    let clip = clip_for(into, &path.content_mask, cx);
    into.push_mono_sprite(
        MonoSprite::new(
            convert::rect(mask.bounds),
            mask.tile,
            convert::color(mask.color),
        )
        .clipped(clip),
    );
}

/// The clip chain for a gpui content mask.
///
/// gpui masks are always axis-aligned rectangles with square corners — rounded clipping is done
/// per-primitive through corner radii rather than through the mask — so one link off the root is
/// always enough.
fn clip_for(into: &mut Scene, mask: &ContentMask<ScaledPixels>, cx: &mut Context<'_>) -> ClipId {
    let key = [
        mask.bounds.origin.x.0.to_bits(),
        mask.bounds.origin.y.0.to_bits(),
        mask.bounds.size.width.0.to_bits(),
        mask.bounds.size.height.0.to_bits(),
    ];
    if let Some((cached, id)) = cx.last_clip
        && cached == key
    {
        return id;
    }
    let id = into.clips.only(ClipLink::rect(convert::rect(mask.bounds)));
    cx.last_clip = Some((key, id));
    id
}

/// A flat fill, reusing the last one when it is the same colour.
fn solid(into: &mut Scene, color: Hsla, cx: &mut Context<'_>) -> PaintRef {
    let key = [
        color.h.to_bits(),
        color.s.to_bits(),
        color.l.to_bits(),
        color.a.to_bits(),
    ];
    if let Some((cached, paint)) = cx.last_solid
        && cached == key
    {
        return paint;
    }
    let paint = into.paints.add(Paint::Solid(convert::color(color)));
    cx.last_solid = Some((key, paint));
    paint
}

/// A clip that also rounds its corners, for the two primitives whose zgui counterpart carries no
/// radii of its own.
fn rounded_clip_for(
    into: &mut Scene,
    mask: &ContentMask<ScaledPixels>,
    bounds: Bounds<ScaledPixels>,
    radii: Corners<ScaledPixels>,
    cx: &mut Context<'_>,
) -> ClipId {
    let outer = clip_for(into, mask, cx);
    if radii.top_left.0 == 0.0
        && radii.top_right.0 == 0.0
        && radii.bottom_right.0 == 0.0
        && radii.bottom_left.0 == 0.0
    {
        return outer;
    }
    into.clips.push(
        outer,
        ClipLink::RoundedRect {
            rect: convert::rect(bounds),
            radii: convert::corner_radii(radii),
            space: SpatialId::VIEWPORT,
        },
    )
}

fn push_quad(into: &mut Scene, quad: &gpui::Quad, cx: &mut Context<'_>) {
    let clip = clip_for(into, &quad.content_mask, cx);
    let fill = background(into, &quad.background, quad.bounds, cx);
    let border = if quad.border_widths.top.0 == 0.0
        && quad.border_widths.right.0 == 0.0
        && quad.border_widths.bottom.0 == 0.0
        && quad.border_widths.left.0 == 0.0
    {
        PaintRef::NONE
    } else {
        solid(into, quad.border_color, cx)
    };

    let mut zquad = Quad::filled(convert::rect(quad.bounds), fill).clipped(clip);
    zquad.radii = convert::corner_radii_array(quad.corner_radii);
    zquad.border = convert::border_widths(quad.border_widths);
    zquad.stroke = border;
    zquad.style = border_style(quad.border_style) as u32;
    into.push_quad(zquad);
}

fn push_shadow(into: &mut Scene, shadow: &gpui::Shadow, cx: &mut Context<'_>) {
    let clip = clip_for(into, &shadow.content_mask, cx);
    let inset = shadow.inset != 0;

    // gpui's `bounds` is the shadow *shape*; its shader dilates by three sigma at vertex time for
    // a drop shadow. zgui expects the painted extent, because that is what it culls and orders by,
    // so the dilation moves here. An inset shadow paints inside its box and is already correct.
    let painted = if inset {
        shadow.bounds
    } else {
        let reach = ScaledPixels(Shadow::BLUR_EXTENT * shadow.blur_radius.0);
        Bounds {
            origin: gpui::Point {
                x: shadow.bounds.origin.x - reach,
                y: shadow.bounds.origin.y - reach,
            },
            size: gpui::Size {
                width: shadow.bounds.size.width + reach + reach,
                height: shadow.bounds.size.height + reach + reach,
            },
        }
    };

    into.push_shadow(Shadow {
        order: 0,
        blur: shadow.blur_radius.0,
        bounds: convert::bounds_array(painted),
        radii: convert::corner_radii_array(shadow.corner_radii),
        element_bounds: convert::bounds_array(shadow.element_bounds),
        element_radii: convert::corner_radii_array(shadow.element_corner_radii),
        color: premultiplied(shadow.color),
        clip: clip.0,
        transform: SpatialId::VIEWPORT.index(),
        inset: u32::from(inset),
        reserved: 0,
    });
}

fn push_underline(into: &mut Scene, underline: &gpui::Underline, cx: &mut Context<'_>) {
    let clip = clip_for(into, &underline.content_mask, cx);
    let style = if underline.wavy.get() {
        DecorationStyle::Wavy
    } else {
        DecorationStyle::Solid
    };
    into.push_decoration(
        Decoration::new(
            convert::rect(underline.bounds),
            underline.thickness.0,
            convert::color(underline.color),
            style,
        )
        .clipped(clip),
    );
}

/// A video or capture surface, as an external texture.
///
/// A surface whose texture this renderer could not adopt is counted rather than drawn: showing
/// nothing is wrong, but showing some other texture's contents would be worse.
fn push_surface(
    into: &mut Scene,
    surface: &gpui::PaintSurface,
    texture: Option<ExternalTextureId>,
    cx: &mut Context<'_>,
) {
    let Some(texture) = texture else {
        cx.missing.foreign_surfaces += 1;
        return;
    };
    let clip = clip_for(into, &surface.content_mask, cx);
    into.push_external(ExternalQuad::new(convert::rect(surface.bounds), texture).clipped(clip));
}

/// A `backdrop-filter`, as zgui's.
///
/// zgui's backdrop carries no corner radii and no opacity of its own, so the radii become a
/// rounded link on its clip chain and the opacity joins the filter chain — which is where CSS puts
/// it anyway.
fn push_backdrop(into: &mut Scene, backdrop: &gpui::BackdropFilter, cx: &mut Context<'_>) {
    let clip = rounded_clip_for(into, &backdrop.content_mask, backdrop.bounds, backdrop.corner_radii, cx);
    let mut filters = filter_chain(&backdrop.filters);
    if backdrop.opacity < 1.0 {
        filters.push(Filter::Opacity(backdrop.opacity));
    }
    into.push_backdrop(BackdropFilter::new(convert::rect(backdrop.bounds), filters).clipped(clip));
}

/// One marker of a `filter` isolation group.
///
/// Markers are matched pairs on both sides and are never culled, so this pushes whatever it is
/// given: dropping a start would leave a render target open, and dropping an end would composite
/// one that was never begun.
fn push_group(into: &mut Scene, boundary: &gpui::FilterBoundary, cx: &mut Context<'_>) {
    let clip = rounded_clip_for(into, &boundary.content_mask, boundary.bounds, boundary.corner_radii, cx);
    // The end marker is derived from a start marker rather than built independently, because zgui
    // wants the pair to agree about bounds, filters and read extent — deriving it is how they
    // cannot drift apart.
    let opening = GroupBoundary::start(
        convert::rect(boundary.bounds),
        boundary.opacity,
        zgui_scene::peniko::BlendMode::default(),
        filter_chain(&boundary.filters),
    );
    let mut marker = if boundary.is_start {
        opening
    } else {
        opening.end()
    };
    marker.clip = clip;
    into.push_group(marker);
}

/// gpui's filter chain, in zgui's vocabulary.
///
/// gpui's [`ScaledFilter`] is deliberately a closed enum so that adding a filter breaks every
/// backend's match rather than silently rendering nothing, which is why this matches exhaustively.
fn filter_chain(filters: &[ScaledFilter]) -> SmallVec<[Filter; 2]> {
    filters
        .iter()
        .map(|filter| match filter {
            // Both measure a blur as a standard deviation in device pixels and both reach three of
            // them, so this is a rename rather than a conversion.
            ScaledFilter::Blur(deviation) => Filter::Blur(deviation.0),
        })
        .collect()
}

/// A gpui background as a zgui paint reference, interned into the scene's paint table.
fn background(
    into: &mut Scene,
    background: &Background,
    bounds: Bounds<ScaledPixels>,
    cx: &mut Context<'_>,
) -> PaintRef {
    match background.kind() {
        BackgroundKind::Solid(color) => solid(into, color, cx),
        BackgroundKind::LinearGradient {
            angle,
            stops,
            color_space,
        } => {
            let (start, end) = linear_gradient(angle, bounds);
            into.paints.add(Paint::Gradient {
                kind: GradientKind::Linear { start, end },
                stops: stops
                    .iter()
                    .map(|stop| GradientStop::new(stop.percentage, convert::color(stop.color)))
                    .collect(),
                space: match color_space {
                    gpui::ColorSpace::Srgb => zgui_color::ColorSpace::Srgb,
                    gpui::ColorSpace::Oklab => zgui_color::ColorSpace::Oklab,
                },
                hue: HueInterpolation::Shorter,
                repeating: false,
            })
        }
        // gpui's two procedural fills have no counterpart here. zgui's paint vocabulary does
        // include a sampled image, which a rasterised pattern tile could have repeated — but its
        // wgpu backend does not implement that variant (`bind/tables.rs` drops the tile and
        // `shader/paint.wgsl` handles only none, solid and gradient), so the tile would sample
        // nothing. Drawing the base colour keeps the element visible and roughly right instead of
        // turning it into a hole. See LIMITATIONS.md.
        BackgroundKind::PatternSlash { color, .. }
        | BackgroundKind::Checkerboard { color, .. } => {
            cx.missing.patterns += 1;
            solid(into, color, cx)
        }
    }
}

/// The endpoints of the gradient line gpui's shader projects onto.
///
/// gpui's linear gradient is not CSS's. Its shader takes a direction from the angle, scales the
/// component along the *longer* axis so the ramp spans the box's aspect ratio, and then normalises
/// `t` by whichever of width or height the direction leans towards. Reproducing it under zgui's
/// plain "project onto the segment" rule is a matter of handing over the two points where that
/// normalised `t` reaches zero and one.
fn linear_gradient(
    angle: f32,
    bounds: Bounds<ScaledPixels>,
) -> (
    zgui_geom::Point<zgui_geom::DevicePx, zgui_geom::Device>,
    zgui_geom::Point<zgui_geom::DevicePx, zgui_geom::Device>,
) {
    let width = bounds.size.width.0;
    let height = bounds.size.height.0;

    // The shader's own expression, including the -90 degrees that makes 0 point to the top.
    let radians = (angle % 360.0 - 90.0).to_radians();
    let mut direction = (radians.cos(), radians.sin());
    if width > height {
        direction.1 *= height / width;
    } else {
        direction.0 *= width / height;
    }

    let length = direction.0.hypot(direction.1);
    let unit = if length > 0.0 {
        (direction.0 / length, direction.1 / length)
    } else {
        (0.0, 1.0)
    };
    // The shader divides the projection by whichever extent the direction leans towards, so that
    // extent is the span the ramp covers.
    let span = if direction.0.abs() > direction.1.abs() {
        width
    } else {
        height
    };

    let center = (
        bounds.origin.x.0 + width / 2.0,
        bounds.origin.y.0 + height / 2.0,
    );
    let half = span / 2.0;
    let point =
        |x: f32, y: f32| zgui_geom::Point::new(zgui_geom::DevicePx(x), zgui_geom::DevicePx(y));
    (
        point(center.0 - unit.0 * half, center.1 - unit.1 * half),
        point(center.0 + unit.0 * half, center.1 + unit.1 * half),
    )
}

fn border_style(style: gpui::BorderStyle) -> BorderStyle {
    match style {
        gpui::BorderStyle::Solid => BorderStyle::Solid,
        gpui::BorderStyle::Dashed => BorderStyle::Dashed,
    }
}

/// A gpui colour in the premultiplied, gamma-encoded sRGB an instance carries directly.
fn premultiplied(color: Hsla) -> [f32; 4] {
    convert::color(color).to_premultiplied_srgb()
}
