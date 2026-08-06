# Known limitations of the zgui backend

Where `gpui_zgui` behaves differently from `gpui_linux`. Each entry says what is lost and why, so
the cost is a decision rather than a surprise.

`~/Projects/zgui` is consumed read-only, so anything marked **zgui gap** cannot be fixed here — it
needs a change upstream in zgui. Everything else is work this crate still owes.

## zgui gaps

### One wgpu device per window
`zgui_render_wgpu::Builder` creates its own `wgpu::Instance` and opens its own device, and offers
no way to build a renderer from an existing one. A surface must come from the instance its device
was opened on, so each window gets a private instance, adapter, device and queue.

*Cost:* a multi-window application pays full GPU-context duplication — driver memory, pipeline
compilation and atlas storage per window. Single-window applications are unaffected.
*Fix:* zgui would need `Builder::from_device` or a shared-`Gpu` constructor.

### `WgpuDeviceRequirements` is ignored
gpui-ce lets an embedder request extra wgpu features and limits before the first window opens
(`Platform::set_gpu_requirements`). zgui picks its own adapter with its own requirements and takes
no input.

*Cost:* an embedder that needs a device feature gpui does not itself request cannot get one.
*Fix:* zgui would need to accept requirements in `Builder`.

### Device-loss semantics differ
`PlatformWindow::gpu_device_lost` has no zgui equivalent. zgui reports loss through
`FrameOutcome::Recovered` after it has already rebuilt the device, rather than exposing a
"currently lost" state an embedder can poll before submitting.

*Cost:* embedders holding the device from `gpu_context()` get no advance warning; they see a
recovered device rather than a lost one. `gpu_device_lost()` always answers `None`.

### `gpu_context()` direction is inverted
Elsewhere in gpui-ce an embedder supplies the device. Here zgui owns it, so `gpu_context()` hands
*out* zgui's `(Arc<Device>, Arc<Queue>)` instead. Embedders must allocate on that device.

*Cost:* an embedder that already has a device cannot make gpui use it. External textures
(`SurfaceSource::Texture`) must be allocated on the device `gpu_context()` returns.

### Patterned fills draw flat
gpui's `BackgroundTag::PatternSlash` and `Checkerboard` are procedural, evaluated per fragment.
zgui's `Paint` has no procedural entry, but it *does* have `Paint::Image { repeating }`, which a
rasterised pattern tile could have been repeated with — except that zgui's wgpu backend does not
implement that variant. `bind/tables.rs:535` lowers `Paint::Image` to a `GpuPaint` with the tile,
destination and transform discarded, and `shader/paint.wgsl` branches on none, solid and gradient
only. A quad filled with an image paint therefore samples nothing and draws nothing.

*Behaviour here:* patterned fills are drawn as their flat base colour and counted, so the element
stays visible rather than becoming a hole.
*Fix:* zgui would need to implement its own image paint. Once it does, the tile-and-repeat approach
works with no gpui change.

### Raster mask clips cannot be applied to a direct draw
zgui's `ClipLink::Mask` exists in the display list, but `bind/tables.rs:454` asserts that a chain
reaching a direct draw carries no mask — a mask is only applied inside a group target.

*Consequence:* a rasterised path mask cannot be used to clip a gradient-filled quad, so a path
whose fill is a gradient is drawn in the ramp's mean colour instead. Solid fills, which are nearly
all of them, are exact.

### Paths are rasterised on the CPU
Not strictly a zgui gap — zgui has vector passes and a vello rasteriser — but they take an
*outline*, and gpui tessellates a path into triangles and discards the outline before the renderer
sees it. Bridging the two properly needs gpui to retain the source outline (and its stroke style
and dash array) alongside the mesh.

*Behaviour here:* the triangle mesh is rasterised into a coverage mask on the CPU and drawn as a
mono sprite. The atlas caches it by content, and the key is measured from the path's own origin, so
a static or merely-translated path rasterises once. A path that changes *shape* every frame is
re-rasterised each time, and one larger than 2048 px on a side is skipped and counted.

## winit gaps

### IME is coarser than gpui's model
gpui's `InputHandler` is modelled on `NSTextInputClient`: marked-text ranges, `bounds_for_range`,
`character_index_for_point`. winit exposes `Ime::{Preedit, Commit}` and `set_ime_cursor_area`.

*Cost:* composition works, but a preedit always replaces the whole marked region rather than a
range the input method names, so some input methods will behave differently from under
`gpui_linux`'s xim / text-input-v3 paths.

### Caps lock state is unavailable
winit reports modifier *keys*, not lock state. `Capslock::on` is always false.

*Cost:* bindings conditioned on caps lock never match.

### No keyboard layout identity
winit applies the layout and exposes no name for it, so `PlatformKeyboardLayout` reports one
unnamed layout and `keyboard_mapper` is gpui's `DummyKeyboardMapper`.

*Cost:* `use_key_equivalents` bindings are the identity, as on gpui's other non-macOS backends.

### Double-click interval is a guess
winit surfaces no system multi-click setting, so click counts are reconstructed from a fixed 500 ms
interval and a 4 px movement threshold rather than the user's preference.

### Monitor identity is derived from the name
winit exposes no stable hardware id. `PlatformDisplay::uuid` is a v5 uuid over the monitor's name,
which survives a restart but not a rename or a replug into a differently-named output.

## Not yet implemented in this crate

These are this backend's work, not upstream gaps.

- Native file dialogs (`prompt_for_paths`, `prompt_for_new_path`) answer `None`.
- Clipboard and primary selection are not wired up.
- Credential storage returns an error.
- Application menus, dock menus and jump lists are ignored.
- Screen capture and system notifications are unimplemented.
- `Platform::restart` is a no-op.
- `active_window()` always answers `None`.
- Window appearance does not follow the system light/dark setting; it is fixed at construction.
