//! The renderer: a [`gpui::Scene`] in, a composed surface out.

mod damage;
mod path;
mod spatial;
mod stats;
mod translate;

pub use crate::renderer::translate::Unsupported;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use zgui_bits::DamageSet;
use zgui_geom::{Scale, Size};
use zgui_render::{FrameOutcome, RenderTarget, Renderer as _};
use zgui_render_wgpu::{Builder, WgpuRenderer, wgpu};
use zgui_scene::Scene;

use crate::atlas::ZguiAtlas;
use crate::renderer::damage::Damage;
use crate::renderer::stats::{Frame, Stats, time};
use crate::renderer::translate::Translator;

/// How much atlas memory to hold before cold rasters are released.
///
/// zgui's atlas does nothing of its own accord without a limit, and gpui's atlases never evict at
/// all, so a long-lived window would otherwise accumulate every glyph variant it had ever drawn.
const ATLAS_SOFT_BYTES: u64 = 64 * 1024 * 1024;

/// Draws gpui scenes through zgui.
pub struct ZguiRenderer {
    renderer: WgpuRenderer,
    scene: Scene,
    atlas: Arc<ZguiAtlas>,
    target: RenderTarget,
    translator: Translator,
    damage: Damage,
    stats: Stats,
    /// Whether damage-based redraw is on. Off restores the whole-surface behaviour gpui's own
    /// renderers have, which is the first thing to try when a visual artefact is reported.
    incremental: bool,
    /// The zgui handle each gpui surface texture was registered under, so a video frame that keeps
    /// the same texture is not re-registered every frame.
    externals: collections::FxHashMap<usize, zgui_scene::ExternalTextureId>,
    /// Handles used this frame, reused as scratch so the surface pass allocates nothing.
    surface_ids: Vec<Option<zgui_scene::ExternalTextureId>>,
    /// The next external handle. Never reused.
    next_external: u64,
    /// What the last frame could not express, so it is reported when it changes rather than once
    /// per frame.
    reported: Unsupported,
}

impl ZguiRenderer {
    /// A renderer presenting to `surface`, which must have come from [`ZguiRenderer::instance`].
    pub fn for_surface(
        builder: Builder,
        surface: wgpu::Surface<'static>,
        size: Size<i32, zgui_geom::Device>,
        scale: f32,
        opaque: bool,
    ) -> Result<Self> {
        let target = RenderTarget {
            size,
            scale: Scale::new(scale),
            opaque,
        };
        let renderer = builder
            .for_surface(target, surface)
            .map_err(|failure| anyhow::anyhow!("no usable graphics device: {failure}"))
            .context("opening a zgui renderer for a window surface")?;
        Ok(Self::wrap(renderer, target))
    }

    /// A renderer drawing to a texture rather than to a window, for tests and screenshots.
    pub fn offscreen(
        size: Size<i32, zgui_geom::Device>,
        scale: f32,
        format: wgpu::TextureFormat,
    ) -> Result<Self> {
        let target = RenderTarget {
            size,
            scale: Scale::new(scale),
            opaque: true,
        };
        let renderer = Builder::new()
            .offscreen(target, format, false)
            .map_err(|failure| anyhow::anyhow!("no usable graphics device: {failure}"))
            .context("opening an offscreen zgui renderer")?;
        Ok(Self::wrap(renderer, target))
    }

    fn wrap(renderer: WgpuRenderer, target: RenderTarget) -> Self {
        let limits = zgui_atlas::AtlasLimits {
            max_texture_size: renderer.capabilities().max_texture_size,
            ..zgui_atlas::AtlasLimits::default()
        }
        .with_soft_bytes(ATLAS_SOFT_BYTES);
        Self {
            renderer,
            scene: Scene::new(),
            atlas: Arc::new(ZguiAtlas::new(limits)),
            target,
            translator: Translator::default(),
            damage: Damage::default(),
            stats: Stats::default(),
            incremental: incremental_enabled(),
            externals: collections::FxHashMap::default(),
            surface_ids: Vec::new(),
            next_external: 0,
            reported: Unsupported::default(),
        }
    }

    /// The sprite atlas gpui fills, shared with the window that owns this renderer.
    pub fn sprite_atlas(&self) -> Arc<ZguiAtlas> {
        self.atlas.clone()
    }

    /// Whether this device can draw per-channel antialiased text.
    pub fn supports_subpixel_text(&self) -> bool {
        self.renderer.capabilities().subpixel_text
    }

    /// The device this renderer opened, so an embedder can allocate textures on it.
    pub fn gpu(&self) -> &Arc<zgui_render_wgpu::Gpu> {
        self.renderer.gpu()
    }

    /// Whether this renderer redraws only what changed.
    pub fn is_incremental(&self) -> bool {
        self.incremental
    }

    /// Turns damage-based redraw on or off.
    ///
    /// Turning it on invalidates, because the composed target's contents are only trustworthy for
    /// frames this renderer itself drew incrementally.
    pub fn set_incremental(&mut self, incremental: bool) {
        self.incremental = incremental;
        self.damage.invalidate();
    }

    /// Resizes the surface, which discards the composed target and forces one full redraw.
    pub fn resize(&mut self, size: Size<i32, zgui_geom::Device>, scale: f32) {
        let target = RenderTarget {
            size,
            scale: Scale::new(scale),
            opaque: self.target.opaque,
        };
        if target == self.target {
            return;
        }
        self.target = target;
        self.renderer.configure(target);
        // The composed target is gone, so nothing outside the new frame's damage would hold the
        // previous frame's pixels.
        self.damage.invalidate();
    }

    /// Marks the start of a frame's atlas activity.
    ///
    /// Called before gpui paints, because painting is what fills the atlas: every glyph gpui emits
    /// looks its raster up, and those lookups are what mark content as still in use.
    pub fn begin_frame(&self) {
        self.atlas.begin_frame();
    }

    /// Translates and draws `scene`.
    pub fn draw(&mut self, scene: &gpui::Scene) -> Result<FrameOutcome> {
        profiling::scope!("ZguiRenderer::draw");

        // Damage is derived before anything is translated, because the cheapest frame is the one
        // that is not built at all: a window that is redrawn while nothing about it changed — an
        // idle repaint, a compositor asking again — damages nothing, and then translating its five
        // thousand primitives only to discover there is nothing to draw is pure waste.
        let measuring = self.stats.enabled();
        let mut frame = Frame::default();

        let damage = time(measuring, &mut frame.compare, || {
            if self.incremental {
                self.damage.damage_for(scene, self.target.size)
            } else {
                DamageSet::full()
            }
        });
        if damage.is_empty() {
            self.damage.retire(true);
            self.stats.skipped();
            return Ok(FrameOutcome::Skipped(zgui_render::SkipReason::Undamaged));
        }

        self.scene.begin_frame(self.target.size);
        self.translator.begin_frame(&mut self.scene);
        self.register_surfaces(scene);
        let missing = time(measuring, &mut frame.translate, || {
            profiling::scope!("translate");
            self.translator
                .translate(scene, &mut self.scene, &self.atlas, &self.surface_ids)
        });
        self.report(missing);

        time(measuring, &mut frame.finish, || self.scene.finish(&damage));

        // Uploads have to reach the device before any pass samples an atlas texture, and the sink
        // only exists behind `&mut` on the renderer, which is why this cannot live in the atlas.
        self.atlas
            .flush_uploads(self.renderer.texture_sink())
            .context("flushing atlas uploads before a frame")?;

        let was_full = damage.is_full();
        let outcome = time(measuring, &mut frame.submit, || {
            self.renderer.draw(&self.scene, &damage)
        });
        if measuring {
            let surface = self.target.size;
            frame.surface = (surface.width as u64) * (surface.height as u64);
            frame.damaged = match &outcome {
                FrameOutcome::Presented(presented) => presented.damage_px,
                _ if was_full => frame.surface,
                _ => damage
                    .rects()
                    .iter()
                    .map(|r| (r.size.width as u64) * (r.size.height as u64))
                    .sum(),
            };
            frame.primitives = scene.quads.len()
                + scene.shadows.len()
                + scene.underlines.len()
                + scene.monochrome_sprites.len()
                + scene.subpixel_sprites.len()
                + scene.polychrome_sprites.len()
                + scene.paths.len();
            self.stats.drawn(frame, was_full);
        }
        // Damage retires on *submission*, not on presentation: a frame that composed into the
        // persistent target and then failed to acquire a surface has still done the work, and
        // redrawing it would repeat what already happened. `retires_damage` is the authority.
        self.damage.retire(outcome.retires_damage());
        Ok(outcome)
    }

    /// The composed target's pixels, for screenshots and for comparing backends.
    pub fn read_composed(&self) -> zgui_render_wgpu::Pixels {
        self.renderer.read_composed()
    }

    /// Adopts every surface texture this frame draws, so the translation can name them.
    ///
    /// gpui hands a surface's texture over as `Arc<dyn Any>`. It is only usable if it is a
    /// `wgpu::Texture` on *this* renderer's device — zgui opens its own, so an embedder that
    /// allocated on some other device produces a texture that cannot be sampled here. That case is
    /// reported by the translation rather than guessed at.
    fn register_surfaces(&mut self, scene: &gpui::Scene) {
        self.surface_ids.clear();
        if scene.surfaces.is_empty() {
            return;
        }
        self.surface_ids.reserve(scene.surfaces.len());

        for (index, surface) in scene.surfaces.iter().enumerate() {
            let Some(texture) = surface_texture(surface) else {
                self.surface_ids.push(None);
                continue;
            };
            // Keyed by index rather than by texture identity: a video surface keeps its place in
            // the scene from frame to frame, and re-registering the same handle is what tells the
            // renderer the contents may have changed.
            let id = *self.externals.entry(index).or_insert_with(|| {
                self.next_external += 1;
                zgui_scene::ExternalTextureId(self.next_external)
            });
            let size = surface.texture_size;
            self.renderer.register_external(zgui_render::ExternalTexture {
                id,
                // Overwritten by `register_external`, which allocates the renderer's own handle.
                handle: zgui_render::TextureHandle(0),
                size: zgui_geom::Size::new(size.width.0, size.height.0),
                premultiplied: true,
            });
            if self.renderer.attach_external(id, &texture) {
                self.surface_ids.push(Some(id));
            } else {
                self.surface_ids.push(None);
            }
        }
    }

    /// Logs what a frame could not express, but only when the totals change.
    fn report(&mut self, missing: Unsupported) {
        if missing == self.reported {
            return;
        }
        self.reported = missing;
        if missing.is_empty() {
            return;
        }
        log::warn!(
            "gpui_zgui: this frame could not draw {} paths or {} surfaces, and drew {} \
             patterned fills flat",
            missing.paths,
            missing.foreign_surfaces,
            missing.patterns,
        );
    }
}

/// The wgpu texture behind a gpui surface, if there is one this renderer can sample.
///
/// gpui hands the texture over type-erased. A downcast that fails means the embedder produced
/// something other than a `wgpu::Texture` — or one from a different device, which is
/// indistinguishable here and shows up as a validation error rather than a wrong picture.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn surface_texture(surface: &gpui::PaintSurface) -> Option<wgpu::Texture> {
    let texture = surface.texture.clone().downcast::<wgpu::Texture>().ok()?;
    Some((*texture).clone())
}

/// Surfaces carry a platform-specific payload, and only the Linux one is a wgpu texture.
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn surface_texture(_surface: &gpui::PaintSurface) -> Option<wgpu::Texture> {
    None
}

/// Whether damage-based redraw is enabled.
///
/// On by default. `GPUI_ZGUI_DAMAGE=0` turns it off, which is the quickest way to tell a damage
/// bug — stale pixels that clear when the window is disturbed — from a translation bug, which
/// looks the same either way.
fn incremental_enabled() -> bool {
    !matches!(
        std::env::var("GPUI_ZGUI_DAMAGE").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}
