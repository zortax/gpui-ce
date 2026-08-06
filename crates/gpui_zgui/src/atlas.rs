//! gpui's sprite atlas, backed by zgui's.
//!
//! The two atlases agree on the shape of the problem — cache a raster under a key, hand back which
//! texture it landed in and where — but differ in three ways that this module reconciles:
//!
//! - **Key vocabulary.** gpui keys by a structured [`gpui::AtlasKey`]; zgui keys by an opaque
//!   `u64` it never interprets. Rather than hashing (where a collision would silently draw the
//!   wrong glyph), every distinct gpui key is handed a fresh sequence number, so the mapping is
//!   injective by construction.
//! - **Eviction.** gpui's atlases never evict; zgui's do, on request. That is safe to drive from
//!   here without any bookkeeping of our own, for two reasons. zgui refuses to evict anything the
//!   current frame has looked up, so the working set is never pulled out from under sprites
//!   already emitted this frame; and a handle whose content *was* evicted simply misses on the
//!   next lookup and is rebuilt under the same handle, so the mapping never has to be retired.
//! - **Pixel order.** gpui rasterises four-channel content as BGRA; zgui's colour pools are RGBA.
//!   Four-byte uploads are swizzled on the way in, exactly as `gpui_wgpu` does.
//!
//! Uploads are queued here and only reach a device in [`ZguiAtlas::flush_uploads`], which the
//! renderer calls once per frame. That is forced by the signatures — [`gpui::PlatformAtlas`]
//! hands out tiles behind `&self`, while writing to a texture needs `&mut` on the device — but it
//! is also what gpui's own backends do.

use std::borrow::Cow;

use anyhow::{Context as _, Result};
use collections::FxHashMap;
use gpui::{AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, DevicePixels, PlatformAtlas, Size, TileId};
use parking_lot::Mutex;
use zgui_atlas::{Atlas, AtlasError, AtlasLimits, TextureKind, TextureSink};

use crate::convert;

/// How many times an out-of-space allocation is retried after evicting cold content.
///
/// One round of eviction frees every unreferenced tile of the pool, so a second failure means the
/// frame genuinely wants more than the pool can hold rather than that it was merely full.
const EVICTION_RETRIES: usize = 2;

/// gpui's [`PlatformAtlas`], over a [`zgui_atlas::Atlas`].
pub struct ZguiAtlas(Mutex<AtlasState>);

struct AtlasState {
    atlas: Atlas,
    /// The zgui handle each gpui key was given.
    ///
    /// Entries are never removed on eviction. A handle whose content went away misses on its next
    /// lookup and is re-inserted under the same handle, so the mapping stays correct; retiring it
    /// would only save the map entry, at the cost of having to observe every eviction.
    handles: FxHashMap<AtlasKey, u64>,
    /// The next unused handle. Never reused, so a handle names one gpui key for the process's life.
    next_handle: u64,
}

impl ZguiAtlas {
    /// An atlas allocating within `limits`.
    pub fn new(limits: AtlasLimits) -> Self {
        Self(Mutex::new(AtlasState {
            atlas: Atlas::new(limits),
            handles: FxHashMap::default(),
            next_handle: 0,
        }))
    }

    /// Marks the start of a frame, so that eviction can tell hot content from cold.
    ///
    /// Also takes the atlas back under its soft byte limit, if it has one. This is the only place
    /// that shrinks it, and it is safe here because nothing has been looked up yet this frame.
    pub fn begin_frame(&self) {
        let mut state = self.0.lock();
        state.atlas.begin_frame();
        state.atlas.evict_to_soft_limit();
    }

    /// Sends everything queued since the last call to the device.
    ///
    /// Called once per frame by the renderer, before any pass reads an atlas texture.
    pub fn flush_uploads(&self, sink: &mut dyn TextureSink) -> Result<u64> {
        let mut state = self.0.lock();
        state
            .atlas
            .flush_uploads(&mut { sink })
            .context("uploading atlas tiles")
    }

    /// Runs `f` against the underlying atlas.
    ///
    /// For content this crate caches itself — pattern tiles — rather than content gpui asked for
    /// through [`PlatformAtlas`].
    pub fn with_atlas<R>(&self, f: impl FnOnce(&mut Atlas) -> R) -> R {
        f(&mut self.0.lock().atlas)
    }

    /// Drops every cached tile, for use when the device was lost and its textures with it.
    pub fn clear(&self) {
        let mut state = self.0.lock();
        state.atlas.clear();
        state.handles.clear();
    }
}

impl AtlasState {
    /// The zgui key for a gpui key, allocating a handle the first time it is asked for.
    fn zgui_key(&mut self, key: &AtlasKey) -> zgui_atlas::AtlasKey {
        let kind = texture_kind(key.texture_kind());
        let handle = match self.handles.get(key) {
            Some(handle) => *handle,
            None => {
                let handle = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(key.clone(), handle);
                handle
            }
        };
        zgui_atlas::AtlasKey::new(handle, kind)
    }

    /// Frees cold content, reporting whether anything was actually released.
    ///
    /// zgui will not evict an entry this frame has looked up, so this can never reclaim a tile
    /// whose position is already baked into a sprite emitted earlier in the same frame. When it
    /// returns `false`, everything resident belongs to the frame being drawn.
    fn evict(&mut self) -> bool {
        self.atlas.evict_all_unused().tiles > 0
    }
}

impl PlatformAtlas for ZguiAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut state = self.0.lock();
        let zgui_key = state.zgui_key(key);

        // A hit is the overwhelmingly common case — every glyph on screen, every frame — so it
        // must not call `build`, which would re-rasterise.
        if let Some(tile) = state.atlas.get(zgui_key) {
            return Ok(Some(tile_for(zgui_key, tile)));
        }

        let Some((size, bytes)) = build()? else {
            return Ok(None);
        };
        let bytes = upload_bytes(zgui_key.kind(), bytes);

        let mut attempt = 0;
        loop {
            // `get_or_insert` takes the bytes by closure and only calls it on a miss, which this
            // is: the `get` above already established that. Cloning here is the price of being
            // able to retry after eviction, and is paid only on a miss.
            let queued = bytes.clone();
            match state
                .atlas
                .get_or_insert(zgui_key, convert::texel_size(size), move || queued)
            {
                Ok(tile) => return Ok(Some(tile_for(zgui_key, tile))),
                Err(AtlasError::OutOfSpace { .. }) if attempt < EVICTION_RETRIES => {
                    if !state.evict() {
                        // Nothing was free to release, so retrying would fail identically.
                        anyhow::bail!(
                            "the {:?} atlas pool is full of content in use this frame",
                            zgui_key.kind()
                        );
                    }
                    attempt += 1;
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error))
                        .context("allocating an atlas tile for a gpui sprite");
                }
            }
        }
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.0.lock();
        let Some(handle) = state.handles.remove(key) else {
            return;
        };
        let kind = texture_kind(key.texture_kind());
        state.atlas.remove(zgui_atlas::AtlasKey::new(handle, kind));
    }
}

impl ZguiAtlas {
    /// Whether `key`'s raster is currently cached.
    ///
    /// Not the `PlatformAtlas` method of the same name: that one only exists when gpui itself is
    /// built with `test-support`, which this crate does not require.
    pub fn is_cached(&self, key: &AtlasKey) -> bool {
        let state = self.0.lock();
        let Some(handle) = state.handles.get(key) else {
            return false;
        };
        let kind = texture_kind(key.texture_kind());
        state
            .atlas
            .contains(zgui_atlas::AtlasKey::new(*handle, kind))
    }
}

/// gpui's pool vocabulary, in zgui's.
fn texture_kind(kind: AtlasTextureKind) -> TextureKind {
    match kind {
        AtlasTextureKind::Monochrome => TextureKind::Mono,
        AtlasTextureKind::Polychrome => TextureKind::Color,
        AtlasTextureKind::Subpixel => TextureKind::Subpixel,
    }
}

/// zgui's pool vocabulary, in gpui's.
fn atlas_texture_kind(kind: TextureKind) -> AtlasTextureKind {
    match kind {
        TextureKind::Mono => AtlasTextureKind::Monochrome,
        TextureKind::Color => AtlasTextureKind::Polychrome,
        TextureKind::Subpixel => AtlasTextureKind::Subpixel,
    }
}

/// A zgui tile, as the tile gpui embeds in a sprite instance.
fn tile_for(key: zgui_atlas::AtlasKey, tile: zgui_atlas::AtlasTile) -> AtlasTile {
    AtlasTile {
        texture_id: AtlasTextureId {
            index: tile.texture.index,
            kind: atlas_texture_kind(key.kind()),
        },
        tile_id: TileId(tile.tile.0),
        padding: 0,
        bounds: convert::device_bounds(tile.bounds),
    }
}

/// The bytes for a pool, in the channel order that pool's format expects.
///
/// gpui rasterises four-channel content as BGRA — the order its own Metal and DirectX textures
/// use — while every zgui four-channel pool is RGBA. Single-channel coverage needs no reordering.
fn upload_bytes(kind: TextureKind, bytes: Cow<'_, [u8]>) -> Vec<u8> {
    let mut bytes = bytes.into_owned();
    if kind.format().bytes_per_texel() == 4 {
        for pixel in bytes.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ImageId, RenderImageParams};

    fn image_key(id: usize) -> AtlasKey {
        AtlasKey::Image(RenderImageParams {
            image_id: ImageId(id),
            frame_index: 0,
        })
    }

    #[test]
    fn a_miss_builds_and_a_hit_does_not() {
        let atlas = ZguiAtlas::new(AtlasLimits::default());
        let key = image_key(1);
        let mut builds = 0;

        for _ in 0..3 {
            let tile = atlas
                .get_or_insert_with(&key, &mut || {
                    builds += 1;
                    Ok(Some((
                        Size {
                            width: DevicePixels(2),
                            height: DevicePixels(2),
                        },
                        Cow::Owned(vec![0u8; 2 * 2 * 4]),
                    )))
                })
                .expect("the tile fits")
                .expect("the build produced content");
            assert_eq!(tile.bounds.size.width, DevicePixels(2));
        }

        assert_eq!(builds, 1, "a cached tile must not be rasterised again");
    }

    #[test]
    fn distinct_keys_never_share_a_tile() {
        let atlas = ZguiAtlas::new(AtlasLimits::default());
        let tile_for = |id: usize| {
            atlas
                .get_or_insert_with(&image_key(id), &mut || {
                    Ok(Some((
                        Size {
                            width: DevicePixels(4),
                            height: DevicePixels(4),
                        },
                        Cow::Owned(vec![0u8; 4 * 4 * 4]),
                    )))
                })
                .expect("the tile fits")
                .expect("the build produced content")
        };
        let first = tile_for(1);
        let second = tile_for(2);
        assert_ne!(first.tile_id, second.tile_id);
    }

    #[test]
    fn a_build_that_declines_produces_no_tile() {
        let atlas = ZguiAtlas::new(AtlasLimits::default());
        let tile = atlas
            .get_or_insert_with(&image_key(7), &mut || Ok(None))
            .expect("declining is not an error");
        assert!(tile.is_none());
    }

    #[test]
    fn removing_a_key_forgets_it() {
        let atlas = ZguiAtlas::new(AtlasLimits::default());
        let key = image_key(9);
        let mut builds = 0;
        let build = |atlas: &ZguiAtlas, builds: &mut i32| {
            atlas
                .get_or_insert_with(&key, &mut || {
                    *builds += 1;
                    Ok(Some((
                        Size {
                            width: DevicePixels(2),
                            height: DevicePixels(2),
                        },
                        Cow::Owned(vec![0u8; 2 * 2 * 4]),
                    )))
                })
                .expect("the tile fits");
        };
        build(&atlas, &mut builds);
        atlas.remove(&key);
        build(&atlas, &mut builds);
        assert_eq!(builds, 2, "a removed tile must be rasterised again");
    }

    #[test]
    fn four_channel_uploads_are_swizzled_to_rgba() {
        // One BGRA pixel: blue=1, green=2, red=3, alpha=4.
        let bytes = upload_bytes(TextureKind::Color, Cow::Owned(vec![1, 2, 3, 4]));
        assert_eq!(bytes, vec![3, 2, 1, 4], "red and blue must be exchanged");
    }

    #[test]
    fn single_channel_uploads_are_left_alone() {
        let bytes = upload_bytes(TextureKind::Mono, Cow::Owned(vec![1, 2, 3, 4]));
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }
}
