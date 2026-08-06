//! Sprite transforms, as coordinate systems.
//!
//! gpui carries a 2×3 affine transform *inside* each sprite instance — it is how a rotated or
//! scaled SVG is drawn. zgui instead names a coordinate system per primitive and keeps the
//! matrices in a tree, so that a thousand rows sharing one transform cost one matrix rather than a
//! thousand.
//!
//! Bridging the two means interning: identical matrices get the same [`SpatialId`]. The tree names
//! nodes after their *owner* rather than their value, precisely so a matrix can change without
//! being renamed, so this keeps its own map from matrix content to the owner it was given. Owners
//! whose matrix went unused for a frame are released, which is what stops an animated transform —
//! a new matrix every frame — from growing the tree without bound.

use collections::FxHashMap;
use gpui::TransformationMatrix;
use zgui_geom::Affine2;
use zgui_scene::{OwnSpace, PropertyOwner, Scene, SpatialId};

/// A matrix's six coefficients, as bits, so it can be a hash key.
///
/// Float bits rather than the floats themselves: `f32` is not `Eq`, and two matrices that differ
/// only in the sign of a zero are the same transform but would hash apart. Normalising the zeroes
/// is what makes the key agree with the transform.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MatrixKey([u32; 6]);

impl MatrixKey {
    fn of(affine: Affine2) -> Self {
        let normalize = |value: f32| (if value == 0.0 { 0.0 } else { value }).to_bits();
        Self([
            normalize(affine.a),
            normalize(affine.b),
            normalize(affine.c),
            normalize(affine.d),
            normalize(affine.tx),
            normalize(affine.ty),
        ])
    }
}

/// Interns gpui sprite transforms into a scene's spatial tree.
#[derive(Default)]
pub struct Transforms {
    /// Which owner each distinct matrix was given.
    owners: FxHashMap<MatrixKey, PropertyOwner>,
    /// Which matrices were asked for this frame, so the rest can be released.
    used: Vec<MatrixKey>,
    /// The next owner handle. Never reused, so a released node cannot be confused with a new one.
    ///
    /// Starts at one because the packed form of a handle is never the empty word, and counts up:
    /// the viewport owns `u64::MAX`, which this would have to run for longer than any process to
    /// reach.
    next: u64,
}

impl Transforms {
    /// Forgets the matrices no frame has used since the last call.
    pub fn begin_frame(&mut self, scene: &mut Scene) {
        if self.owners.len() > self.used.len() {
            let live: collections::FxHashSet<MatrixKey> = self.used.iter().copied().collect();
            self.owners.retain(|key, owner| {
                let keep = live.contains(key);
                if !keep {
                    scene.spatial.release(*owner);
                }
                keep
            });
        }
        self.used.clear();
    }

    /// The coordinate system for a gpui sprite transform.
    ///
    /// The identity — which is nearly every sprite, every glyph in a paragraph — is the viewport
    /// itself and costs no node at all.
    pub fn id_for(&mut self, scene: &mut Scene, transform: &TransformationMatrix) -> SpatialId {
        if *transform == TransformationMatrix::unit() {
            return scene.spatial.viewport();
        }

        let affine = affine_of(transform);
        let key = MatrixKey::of(affine);
        let owner = match self.owners.get(&key) {
            Some(owner) => *owner,
            None => {
                self.next += 1;
                let owner = PropertyOwner::new(self.next)
                    .expect("a counter starting above zero never packs to the empty word");
                self.owners.insert(key, owner);
                owner
            }
        };
        self.used.push(key);

        let viewport = scene.spatial.viewport();
        scene.spatial.space_of(
            viewport,
            owner,
            OwnSpace::of(Some(affine.to_matrix4()), None, false),
        )
    }
}

/// gpui's row-major 2×2-plus-translation, as the six coefficients CSS `matrix()` writes.
///
/// gpui applies it as `out[i] = translation[i] + Σ rotation_scale[i][k] · in[k]`, so its first row
/// produces `x` and its second produces `y`. CSS's order is column-major by comparison — `a` and
/// `b` are both coefficients *of* `x` — which is why the two middle terms cross over here.
fn affine_of(transform: &TransformationMatrix) -> Affine2 {
    let rows = transform.rotation_scale;
    Affine2::new(
        rows[0][0],
        rows[1][0],
        rows[0][1],
        rows[1][1],
        transform.translation[0],
        transform.translation[1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Point, ScaledPixels};

    fn point(x: f32, y: f32) -> Point<gpui::Pixels> {
        Point {
            x: gpui::px(x),
            y: gpui::px(y),
        }
    }

    /// The conversion is right exactly when both libraries move a point to the same place.
    #[track_caller]
    fn agrees_on(transform: TransformationMatrix, x: f32, y: f32) {
        let theirs = transform.apply(point(x, y));
        let ours = affine_of(&transform);
        let mapped_x = ours.a * x + ours.c * y + ours.tx;
        let mapped_y = ours.b * x + ours.d * y + ours.ty;
        assert!(
            (f32::from(theirs.x) - mapped_x).abs() < 1e-4
                && (f32::from(theirs.y) - mapped_y).abs() < 1e-4,
            "gpui maps ({x}, {y}) to {theirs:?}, the affine maps it to ({mapped_x}, {mapped_y})"
        );
    }

    #[test]
    fn a_translation_agrees_with_gpui() {
        let transform = TransformationMatrix::unit().translate(Point {
            x: ScaledPixels(7.0),
            y: ScaledPixels(-3.0),
        });
        agrees_on(transform, 0.0, 0.0);
        agrees_on(transform, 5.0, 11.0);
    }

    #[test]
    fn a_rotation_agrees_with_gpui() {
        // A quarter turn is the case that catches a transposed matrix: it maps the x axis onto
        // the y axis, so swapping the two middle coefficients sends the point the wrong way.
        let transform = TransformationMatrix::unit().rotate(gpui::Radians(std::f32::consts::FRAC_PI_2));
        agrees_on(transform, 1.0, 0.0);
        agrees_on(transform, 0.0, 1.0);
        agrees_on(transform, 3.0, 5.0);
    }

    #[test]
    fn a_scale_then_translation_agrees_with_gpui() {
        let transform = TransformationMatrix::unit()
            .scale(gpui::Size {
                width: 2.0,
                height: 3.0,
            })
            .translate(Point {
                x: ScaledPixels(4.0),
                y: ScaledPixels(6.0),
            });
        agrees_on(transform, 1.0, 1.0);
        agrees_on(transform, -2.0, 8.0);
    }

    #[test]
    fn equal_matrices_share_one_coordinate_system() {
        let mut transforms = Transforms::default();
        let mut scene = Scene::new();
        scene.begin_frame(zgui_geom::Size::new(64, 64));
        let rotated =
            TransformationMatrix::unit().rotate(gpui::Radians(std::f32::consts::FRAC_PI_4));

        let first = transforms.id_for(&mut scene, &rotated);
        let again = transforms.id_for(&mut scene, &rotated);
        assert_eq!(first, again, "one matrix names one coordinate system");
    }

    #[test]
    fn the_identity_costs_no_node() {
        let mut transforms = Transforms::default();
        let mut scene = Scene::new();
        scene.begin_frame(zgui_geom::Size::new(64, 64));
        let id = transforms.id_for(&mut scene, &TransformationMatrix::unit());
        assert_eq!(id, scene.spatial.viewport());
    }
}
