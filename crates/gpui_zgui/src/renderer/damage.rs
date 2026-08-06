//! Working out what actually changed, so the renderer can redraw only that.
//!
//! This is the whole reason for the backend. gpui rebuilds its scene from scratch every frame and
//! its own renderers clear and redraw the entire window; zgui composes into a target it keeps
//! between frames and scissors each frame's passes to a damage set. What is missing in between is
//! any statement of what changed — so this derives one by comparing the frame gpui just built
//! against the one before it.
//!
//! # How the comparison works
//!
//! Every primitive is reduced to a hash of its bytes and the rectangle it paints. Two frames are
//! then compared as *multisets* of those hashes: a hash whose count differs between the frames
//! marks every primitive carrying it as changed, in both frames, and their rectangles are what
//! gets damaged.
//!
//! Comparing multisets rather than array positions is what makes an insertion cheap. Adding one
//! quad shifts every later element of gpui's sorted arrays, so a positional comparison would
//! report the whole tail as changed; a multiset notices exactly the one that appeared.
//!
//! # Why the draw order is part of the hash
//!
//! It is tempting to exclude `order`, because an element that moves can cascade fresh orders onto
//! primitives that look identical, and each of those then counts as changed. But consider two
//! overlapping primitives that swap which is on top without either changing otherwise: the
//! multiset of everything-except-order is unchanged, so nothing would be damaged, and the frame
//! would keep last frame's stacking. Including the order costs over-damage when geometry moves —
//! which is a frame that was going to be expensive anyway — and buys soundness in the case that
//! silently draws the wrong picture.
//!
//! # What is conservative here
//!
//! Over-damaging is always safe and only costs time; under-damaging leaves stale pixels on the
//! screen. Every judgement call below is made in that direction: surfaces always damage, filters
//! grow damage to whatever they read, and anything unusual falls back to the whole surface.

use collections::FxHashMap;
use gpui::{Bounds, ScaledPixels};
use zgui_bits::DamageSet;
use zgui_geom::{Device, Rect, Size};

/// Above this many changed primitives, the whole surface is redrawn instead.
///
/// The set holds four rectangles, so a large scatter has already merged into something close to
/// the full surface; past that point the comparison is paying for a redraw it is not avoiding.
const MAX_CHANGED: usize = 2048;

/// Above this fraction of the surface, the whole surface is redrawn instead.
///
/// Scissored passes are not free — each rectangle is one more replay of the batch stream — so
/// damage that already covers most of the window is cheaper drawn as one full pass.
const MAX_DAMAGED_FRACTION: f32 = 0.7;

/// How many times filter read-extents are propagated before giving up and going full.
///
/// A backdrop filter reads what is beneath it, so damaging it can damage another backdrop above
/// it. Chains that deep are not real, and bounding the loop is cheaper than proving it terminates.
const FILTER_ROUNDS: usize = 4;

/// One primitive, reduced to what the comparison needs.
#[derive(Clone, Copy)]
struct Entry {
    /// A hash of every byte of the primitive, including its draw order.
    hash: u64,
    /// The rectangle it paints, already clipped and rounded out to whole pixels.
    rect: Rect<i32, Device>,
}

/// A region a filter reads from, and the region whose output changes when it does.
#[derive(Clone, Copy)]
struct Reader {
    /// What the filter samples.
    source: Rect<i32, Device>,
    /// What it writes, which must be redrawn when anything in `source` is.
    writes: Rect<i32, Device>,
}

/// Everything one frame contributes to the comparison.
#[derive(Default)]
struct Snapshot {
    entries: Vec<Entry>,
    readers: Vec<Reader>,
    /// Rectangles that must be damaged whatever the comparison says.
    ///
    /// External surfaces live here: a video frame's texture changes without the primitive
    /// describing it changing at all, so nothing about the scene would reveal it.
    always: Vec<Rect<i32, Device>>,
}

/// Derives a damage set for each frame from the one before it.
#[derive(Default)]
pub struct Damage {
    previous: Option<Snapshot>,
    /// Damage carried over from frames whose work was not submitted.
    pending: DamageSet,
    /// Set when something outside the scene invalidated the whole surface.
    force_full: bool,
    /// Scratch, reused so a frame's comparison allocates nothing.
    counts: FxHashMap<u64, i32>,
    scratch: Snapshot,
}

impl Damage {
    /// Declares that the next frame must redraw everything.
    ///
    /// The renderer already forces a full frame after a resize or a device loss, so this is for
    /// the cases it cannot see — chiefly the first frame drawn against a new comparison.
    pub fn invalidate(&mut self) {
        self.force_full = true;
    }

    /// The damage for `scene`, given everything drawn before it.
    pub fn damage_for(&mut self, scene: &gpui::Scene, viewport: Size<i32, Device>) -> DamageSet {
        profiling::scope!("damage");

        let surface = Rect::new(zgui_geom::Point::new(0, 0), viewport);
        let mut frame = std::mem::take(&mut self.scratch);
        frame.clear();
        snapshot(scene, surface, &mut frame);

        // `for_frame` rather than `new`, so zgui's own full-damage override still works.
        let mut damage = DamageSet::for_frame();
        let previous = self.previous.take();

        match previous {
            // Nothing to compare against, so nothing can be reused.
            None => damage.set_full(),
            Some(_) if self.force_full => damage.set_full(),
            Some(previous) => {
                self.compare(&previous, &frame, &mut damage, surface);
            }
        }

        self.force_full = false;
        self.pending.absorb_set(&damage);
        self.scratch = std::mem::replace(&mut self.previous, Some(frame)).unwrap_or_default();
        self.pending
    }

    /// Records that a frame's work reached the target, so its damage need not be redrawn.
    pub fn retire(&mut self, retired: bool) {
        if retired {
            self.pending = DamageSet::new();
        }
    }

    /// Fills `damage` with everything that differs between two frames.
    fn compare(
        &mut self,
        previous: &Snapshot,
        frame: &Snapshot,
        damage: &mut DamageSet,
        surface: Rect<i32, Device>,
    ) {
        self.counts.clear();
        self.counts.reserve(frame.entries.len());
        for entry in &previous.entries {
            *self.counts.entry(entry.hash).or_default() += 1;
        }
        for entry in &frame.entries {
            *self.counts.entry(entry.hash).or_default() -= 1;
        }

        // A hash whose count did not balance appeared or disappeared, so every primitive carrying
        // it is treated as changed in both frames: the pixels it used to cover and the ones it
        // covers now both have to be redrawn.
        let mut changed = 0;
        for entry in previous.entries.iter().chain(&frame.entries) {
            if self.counts.get(&entry.hash).copied().unwrap_or(0) != 0 {
                changed += 1;
                if changed > MAX_CHANGED {
                    damage.set_full();
                    return;
                }
                damage.absorb(entry.rect);
            }
        }

        for rect in &frame.always {
            damage.absorb(*rect);
        }

        grow_for_filters(damage, &frame.readers);

        if damage.is_full() {
            return;
        }
        // Past a point, one full pass beats several scissored ones that between them cover nearly
        // as much.
        let damaged: i64 = damage
            .rects()
            .iter()
            .map(|rect| i64::from(rect.size.width) * i64::from(rect.size.height))
            .sum();
        let total = i64::from(surface.size.width) * i64::from(surface.size.height);
        if total > 0 && damaged as f32 > total as f32 * MAX_DAMAGED_FRACTION {
            damage.set_full();
        }
    }
}

impl Snapshot {
    fn clear(&mut self) {
        self.entries.clear();
        self.readers.clear();
        self.always.clear();
    }
}

/// Grows `damage` until it covers everything every filter reads.
///
/// A filter that samples outside what it writes — a blur, a drop shadow, a backdrop — produces a
/// different result when the pixels it reads change, even though nothing about the filter itself
/// did. zgui's own documentation is blunt about the consequence of missing this: a frosted panel
/// that reads a region which was not redrawn re-reads its own previous output, and smears a little
/// further every frame until the whole panel is fog.
///
/// The loop is because one filter's output can be another's input.
fn grow_for_filters(damage: &mut DamageSet, readers: &[Reader]) {
    if readers.is_empty() || damage.is_full() || damage.is_empty() {
        return;
    }
    for _ in 0..FILTER_ROUNDS {
        let mut grew = false;
        for reader in readers {
            if damage.intersects(reader.source) && !damage.contains(reader.writes) {
                damage.absorb(reader.writes);
                grew = true;
            }
        }
        if !grew {
            return;
        }
    }
    // Still growing after several rounds: the chain is deeper than this is willing to walk, and
    // guessing wrong here is a visible artefact rather than a slow frame.
    damage.set_full();
}

/// Reduces a gpui scene to what the comparison needs.
fn snapshot(scene: &gpui::Scene, surface: Rect<i32, Device>, into: &mut Snapshot) {
    let capacity = scene.quads.len()
        + scene.shadows.len()
        + scene.underlines.len()
        + scene.monochrome_sprites.len()
        + scene.subpixel_sprites.len()
        + scene.polychrome_sprites.len();
    into.entries.reserve(capacity);

    // Every one of these is `#[repr(C)]` and free of padding by construction — that is what
    // `PaddedBool32` and the explicit `pad` fields in gpui's scene are for — so hashing their
    // bytes is well defined and hashes exactly what the GPU will read.
    for quad in &scene.quads {
        push(into, surface, bytes_of(quad), clipped(quad.bounds, quad.content_mask.bounds));
    }
    for underline in &scene.underlines {
        push(
            into,
            surface,
            bytes_of(underline),
            clipped(underline.bounds, underline.content_mask.bounds),
        );
    }
    for sprite in &scene.monochrome_sprites {
        push(
            into,
            surface,
            bytes_of(sprite),
            clipped(sprite.bounds, sprite.content_mask.bounds),
        );
    }
    for sprite in &scene.subpixel_sprites {
        push(
            into,
            surface,
            bytes_of(sprite),
            clipped(sprite.bounds, sprite.content_mask.bounds),
        );
    }
    for sprite in &scene.polychrome_sprites {
        push(
            into,
            surface,
            bytes_of(sprite),
            clipped(sprite.bounds, sprite.content_mask.bounds),
        );
    }
    for shadow in &scene.shadows {
        // The rectangle a shadow paints is not the one it stores: a drop shadow reaches three
        // standard deviations past its shape, exactly as the translation dilates it.
        let painted = if shadow.inset != 0 {
            shadow.bounds
        } else {
            dilate(shadow.bounds, zgui_scene::Shadow::BLUR_EXTENT * shadow.blur_radius.0)
        };
        push(
            into,
            surface,
            bytes_of(shadow),
            clipped(painted, shadow.content_mask.bounds),
        );
    }

    // A path is not translated yet, so it paints nothing and can contribute nothing. Were it to
    // start painting without being counted here, its pixels would go stale.
    for path in &scene.paths {
        push(
            into,
            surface,
            &hash_path(path).to_le_bytes(),
            clipped(path.bounds, path.content_mask.bounds),
        );
    }

    // A surface's texture is filled by something outside this scene — a video decoder — so the
    // primitive describing it is identical from frame to frame while the pixels change underneath.
    // Nothing in the comparison can see that, so it is declared unconditionally.
    for surface_primitive in &scene.surfaces {
        let rect = round_out(clipped(
            surface_primitive.bounds,
            surface_primitive.content_mask.bounds,
        ));
        if let Some(rect) = rect.intersection(surface) {
            into.always.push(rect);
        }
    }

    for backdrop in &scene.backdrop_filters {
        let writes = clipped(backdrop.bounds, backdrop.content_mask.bounds);
        push_reader(into, surface, writes, &backdrop.filters);
        push(
            into,
            surface,
            &hash_filters(backdrop.order, backdrop.bounds, backdrop.opacity, &backdrop.filters)
                .to_le_bytes(),
            writes,
        );
    }
    for boundary in &scene.filter_boundaries {
        let writes = clipped(boundary.bounds, boundary.content_mask.bounds);
        // A group reads its own contents, so anything changing inside it changes what it
        // composites — and a blur inside reads past the group's edge as well.
        push_reader(into, surface, writes, &boundary.filters);
        push(
            into,
            surface,
            &hash_filters(boundary.order, boundary.bounds, boundary.opacity, &boundary.filters)
                .to_le_bytes(),
            writes,
        );
    }
}

/// Records that whatever is drawn in `writes` depends on a region inflated by `filters`.
fn push_reader(
    into: &mut Snapshot,
    surface: Rect<i32, Device>,
    writes: Bounds<ScaledPixels>,
    filters: &[gpui::ScaledFilter],
) {
    let reach = filters
        .iter()
        .map(|filter| match filter {
            gpui::ScaledFilter::Blur(deviation) => {
                zgui_scene::Shadow::BLUR_EXTENT * deviation.0.max(0.0)
            }
        })
        .fold(0.0f32, f32::max);

    let Some(written) = round_out(writes).intersection(surface) else {
        return;
    };
    let Some(source) = round_out(dilate(writes, reach)).intersection(surface) else {
        return;
    };
    into.readers.push(Reader {
        source,
        writes: written,
    });
}

fn push(into: &mut Snapshot, surface: Rect<i32, Device>, bytes: &[u8], bounds: Bounds<ScaledPixels>) {
    let Some(rect) = round_out(bounds).intersection(surface) else {
        // Nothing on the surface: it cannot change any pixel, so it cannot damage one either.
        return;
    };
    into.entries.push(Entry {
        hash: hash_bytes(bytes),
        rect,
    });
}

/// The part of `bounds` its content mask admits.
fn clipped(bounds: Bounds<ScaledPixels>, mask: Bounds<ScaledPixels>) -> Bounds<ScaledPixels> {
    bounds.intersect(&mask)
}

fn dilate(bounds: Bounds<ScaledPixels>, by: f32) -> Bounds<ScaledPixels> {
    let by = ScaledPixels(by);
    Bounds {
        origin: gpui::Point {
            x: bounds.origin.x - by,
            y: bounds.origin.y - by,
        },
        size: gpui::Size {
            width: bounds.size.width + by + by,
            height: bounds.size.height + by + by,
        },
    }
}

/// A fractional rectangle as the whole pixels that can contain it.
///
/// Rounded outwards rather than to nearest: a rectangle that ends halfway through a pixel still
/// changed that pixel, and rounding it away is exactly the kind of one-pixel-stale artefact this
/// whole mechanism exists to avoid.
fn round_out(bounds: Bounds<ScaledPixels>) -> Rect<i32, Device> {
    let left = bounds.origin.x.0.floor() as i32;
    let top = bounds.origin.y.0.floor() as i32;
    let right = (bounds.origin.x.0 + bounds.size.width.0).ceil() as i32;
    let bottom = (bounds.origin.y.0 + bounds.size.height.0).ceil() as i32;
    Rect::new(
        zgui_geom::Point::new(left, top),
        Size::new((right - left).max(0), (bottom - top).max(0)),
    )
}

/// The bytes of a `#[repr(C)]` primitive.
///
/// Safety rests on gpui's scene primitives being padding-free: each carries explicit `pad` fields
/// and uses `PaddedBool32` rather than `bool` precisely so that they can be reinterpreted as bytes
/// for the instance buffers the GPU reads. Hashing the same bytes the GPU will read is what makes
/// "the hash did not change" mean "the pixels will not change".
fn bytes_of<T>(value: &T) -> &[u8] {
    // Safety: `T` is one of gpui's `#[repr(C)]` scene primitives, which contain no padding and no
    // references, and the slice borrows `value` for its own lifetime.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast::<u8>(), size_of::<T>()) }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = collections::FxHasher::default();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A path's identity, for frames where paths are not drawn but must still be tracked.
fn hash_path(path: &gpui::Path<ScaledPixels>) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = collections::FxHasher::default();
    path.order.hash(&mut hasher);
    for vertex in &path.vertices {
        vertex.xy_position.x.0.to_bits().hash(&mut hasher);
        vertex.xy_position.y.0.to_bits().hash(&mut hasher);
        vertex.st_position.x.to_bits().hash(&mut hasher);
        vertex.st_position.y.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// A filter primitive's identity. Its chain is a `SmallVec`, so it cannot be hashed as bytes.
fn hash_filters(
    order: gpui::DrawOrder,
    bounds: Bounds<ScaledPixels>,
    opacity: f32,
    filters: &[gpui::ScaledFilter],
) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = collections::FxHasher::default();
    order.hash(&mut hasher);
    for value in [
        bounds.origin.x.0,
        bounds.origin.y.0,
        bounds.size.width.0,
        bounds.size.height.0,
        opacity,
    ] {
        value.to_bits().hash(&mut hasher);
    }
    for filter in filters {
        match filter {
            gpui::ScaledFilter::Blur(deviation) => {
                0u8.hash(&mut hasher);
                deviation.0.to_bits().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}
