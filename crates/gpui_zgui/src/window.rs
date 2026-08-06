//! gpui's [`PlatformWindow`], over a winit window and a zgui renderer.
//!
//! Almost every method of the trait takes `&self`, because gpui holds windows behind shared
//! handles and calls into them from callbacks it is already inside. So the mutable parts — the
//! renderer, the registered callbacks, the pointer state — live in [`RefCell`]s, and the rule is
//! that no borrow is ever held across a call back into gpui.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use futures::channel::oneshot;
use gpui::{
    Bounds, Capslock, CursorStyle, DispatchEventResult, GpuSpecs, Modifiers, Pixels, PlatformAtlas,
    PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance, WindowBackgroundAppearance,
    WindowBounds, WindowControlArea, px,
};
use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, HandleError, WindowHandle,
};
use zgui_geom::Size as ZSize;
use zgui_render::FrameOutcome;

use crate::atlas::ZguiAtlas;
use crate::display::ZguiDisplay;
use crate::input::Clicks;
use crate::renderer::ZguiRenderer;

/// The callbacks gpui registers on a window.
#[derive(Default)]
pub struct Callbacks {
    pub request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub hover_status_change: Option<Box<dyn FnMut(bool)>>,
    pub resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub moved: Option<Box<dyn FnMut()>>,
    pub should_close: Option<Box<dyn FnMut() -> bool>>,
    pub close: Option<Box<dyn FnOnce()>>,
    pub appearance_changed: Option<Box<dyn FnMut()>>,
    pub hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
}

/// The pointer and keyboard state a window has to remember between events.
#[derive(Default)]
pub struct InputState {
    pub mouse_position: Point<Pixels>,
    pub modifiers: Modifiers,
    pub capslock: Capslock,
    pub clicks: Clicks,
    /// Which button is held, for the `pressed_button` every move event carries.
    pub pressed_button: Option<gpui::MouseButton>,
}

/// Everything a window owns that is mutated after construction.
pub struct WindowState {
    pub renderer: ZguiRenderer,
    pub callbacks: Callbacks,
    pub input: InputState,
    pub input_handler: Option<PlatformInputHandler>,
    pub display: Rc<ZguiDisplay>,
    pub background_appearance: WindowBackgroundAppearance,
    pub appearance: WindowAppearance,
}

/// Everything a window is, behind one allocation.
struct WindowShared {
    window: Arc<winit::window::Window>,
    state: RefCell<WindowState>,
    atlas: Arc<ZguiAtlas>,
    /// Whether the renderer said further frames cannot help until something external changes.
    ///
    /// This is only ever a reason to *stop* asking for frames. A presented frame says nothing
    /// about whether another one is wanted — treating it as a request is an unbounded redraw
    /// loop, which is what this field used to cause.
    frames_suppressed: Cell<bool>,
}

/// A handle to a gpui window drawn by zgui.
///
/// Cloning one is cheap and shares the window: gpui owns a `Box<dyn PlatformWindow>` while the
/// event loop keeps its own handle to route events by winit id, and the window goes away when the
/// last handle does. That is also why every trait method works through a `RefCell` rather than
/// `&mut self` — two live handles must both be able to reach the state.
#[derive(Clone)]
pub struct ZguiWindow(Rc<WindowShared>);

impl ZguiWindow {
    /// Wraps a winit window and the renderer drawing into it.
    pub fn new(
        window: Arc<winit::window::Window>,
        renderer: ZguiRenderer,
        display: Rc<ZguiDisplay>,
        appearance: WindowAppearance,
    ) -> Self {
        let atlas = renderer.sprite_atlas();
        Self(Rc::new(WindowShared {
            window,
            state: RefCell::new(WindowState {
                renderer,
                callbacks: Callbacks::default(),
                input: InputState::default(),
                input_handler: None,
                display,
                background_appearance: WindowBackgroundAppearance::Opaque,
                appearance,
            }),
            atlas,
            frames_suppressed: Cell::new(false),
        }))
    }

    /// Whether asking for another frame would be pointless right now.
    ///
    /// True while the window is occluded or its device could not be rebuilt. Cleared by anything
    /// that could change that, which is any event at all.
    pub fn frames_suppressed(&self) -> bool {
        self.0.frames_suppressed.get()
    }

    /// Allows frames to be requested again.
    pub fn resume_frames(&self) {
        self.0.frames_suppressed.set(false);
    }

    /// The winit window this draws into.
    pub fn winit(&self) -> &Arc<winit::window::Window> {
        &self.0.window
    }

    /// Borrows the mutable state.
    ///
    /// Callers must not call back into gpui while holding it.
    pub fn state(&self) -> std::cell::RefMut<'_, WindowState> {
        self.0.state.borrow_mut()
    }

    /// Delivers an input event to gpui, returning whether the platform should treat it as handled.
    ///
    /// A key press that nothing consumed and that would type a character is inserted here, by
    /// handing the text to the window's input handler. That is the platform's job rather than
    /// gpui's — gpui core only inserts text on its keystroke-replay path, so a backend that
    /// merely dispatches the event and stops leaves every field unable to be typed into. gpui's
    /// own Linux backends do exactly this, and the conditions match theirs: the event must have
    /// propagated, and the modifiers must be no more than shift, so that `ctrl-a` runs its binding
    /// instead of typing an `a`.
    pub fn dispatch_input(&self, event: PlatformInput) -> DispatchEventResult {
        // Taken out and put back so the window is not borrowed while gpui runs, which it must not
        // be: handling an event routinely calls back into the window.
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.input.take() else {
            return DispatchEventResult::default();
        };
        let result = callback(event.clone());
        self.0.state.borrow_mut().callbacks.input = Some(callback);
        if result.propagate {
            self.insert_typed_text(&event);
        }
        result
    }

    /// Inserts the character a key press would type, if it would type one.
    fn insert_typed_text(&self, event: &PlatformInput) {
        let PlatformInput::KeyDown(key_down) = event else {
            return;
        };
        if !key_down
            .keystroke
            .modifiers
            .is_subset_of(&gpui::Modifiers::shift())
        {
            return;
        }
        let Some(text) = key_down.keystroke.key_char.clone() else {
            return;
        };
        // Taken out for the same reason as the callback above: inserting text calls back into the
        // window to ask where the selection is.
        let Some(mut handler) = self.0.state.borrow_mut().input_handler.take() else {
            return;
        };
        handler.replace_text_in_range(None, &text);
        self.0.state.borrow_mut().input_handler = Some(handler);
    }

    /// Runs gpui's frame callback, which is what makes it draw.
    pub fn request_frame(&self, options: RequestFrameOptions) {
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.request_frame.take() else {
            return;
        };
        callback(options);
        self.0.state.borrow_mut().callbacks.request_frame = Some(callback);
    }

    /// Tells gpui the window changed size.
    pub fn resized(&self, size: Size<Pixels>, scale: f32) {
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.resize.take() else {
            return;
        };
        callback(size, scale);
        self.0.state.borrow_mut().callbacks.resize = Some(callback);
    }

    /// Tells gpui the window gained or lost focus.
    pub fn active_status_changed(&self, active: bool) {
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.active_status_change.take()
        else {
            return;
        };
        callback(active);
        self.0.state.borrow_mut().callbacks.active_status_change = Some(callback);
    }

    /// Tells gpui the pointer entered or left the window.
    pub fn hover_status_changed(&self, hovered: bool) {
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.hover_status_change.take()
        else {
            return;
        };
        callback(hovered);
        self.0.state.borrow_mut().callbacks.hover_status_change = Some(callback);
    }

    /// Asks gpui whether the window may close.
    pub fn should_close(&self) -> bool {
        let Some(mut callback) = self.0.state.borrow_mut().callbacks.should_close.take() else {
            return true;
        };
        let close = callback();
        self.0.state.borrow_mut().callbacks.should_close = Some(callback);
        close
    }

    /// Tells gpui the window is going away. Runs at most once.
    pub fn closed(&self) {
        let Some(callback) = self.0.state.borrow_mut().callbacks.close.take() else {
            return;
        };
        callback();
    }

    /// Reconfigures the renderer for a new surface size.
    pub fn surface_resized(&self, width: u32, height: u32, scale: f32) {
        self.0.state
            .borrow_mut()
            .renderer
            .resize(ZSize::new(width as i32, height as i32), scale);
    }

    /// Marks the start of a frame, before gpui paints into it.
    pub fn begin_frame(&self) {
        self.0.state.borrow().renderer.begin_frame();
    }
}

impl HasWindowHandle for ZguiWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.0.window.window_handle()
    }
}

impl HasDisplayHandle for ZguiWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.0.window.display_handle()
    }
}

impl PlatformWindow for ZguiWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        let scale = self.scale_factor();
        let position = self
            .0
            .window
            .outer_position()
            .unwrap_or(winit::dpi::PhysicalPosition::new(0, 0));
        Bounds {
            origin: Point {
                x: px(position.x as f32 / scale),
                y: px(position.y as f32 / scale),
            },
            size: self.content_size(),
        }
    }

    fn is_maximized(&self) -> bool {
        self.0.window.is_maximized()
    }

    fn window_bounds(&self) -> WindowBounds {
        if self.is_fullscreen() {
            WindowBounds::Fullscreen(self.bounds())
        } else if self.is_maximized() {
            WindowBounds::Maximized(self.bounds())
        } else {
            WindowBounds::Windowed(self.bounds())
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        let scale = self.scale_factor();
        let size = self.0.window.inner_size();
        Size {
            width: px(size.width as f32 / scale),
            height: px(size.height as f32 / scale),
        }
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let scale = self.scale_factor();
        let _ = self
            .0
            .window
            .request_inner_size(winit::dpi::PhysicalSize::new(
                (f32::from(size.width) * scale) as u32,
                (f32::from(size.height) * scale) as u32,
            ));
    }

    fn scale_factor(&self) -> f32 {
        self.0.window.scale_factor() as f32
    }

    fn appearance(&self) -> WindowAppearance {
        self.0.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.state.borrow().display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.state.borrow().input.mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.0.state.borrow().input.modifiers
    }

    fn capslock(&self) -> Capslock {
        self.0.state.borrow().input.capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // Returning `None` is how a platform says "no native dialog here", and gpui renders the
        // prompt itself. That is better than a half-native dialog, and it is what the Linux
        // backends do too.
        None
    }

    fn activate(&self) {
        self.0.window.focus_window();
    }

    fn request_attention(&self) {
        self.0.window
            .request_user_attention(Some(winit::window::UserAttentionType::Informational));
    }

    fn is_active(&self) -> bool {
        self.0.window.has_focus()
    }

    fn is_hovered(&self) -> bool {
        // winit reports enter and leave rather than a queryable state, so this follows the last
        // pointer event the window saw.
        self.0.state.borrow().input.mouse_position.x >= px(0.0)
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.0.state.borrow().background_appearance
    }

    fn set_title(&mut self, title: &str) {
        self.0.window.set_title(title);
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        self.0.state.borrow_mut().background_appearance = background_appearance;
        let opaque = background_appearance == WindowBackgroundAppearance::Opaque;
        self.0.window.set_transparent(!opaque);
    }

    fn minimize(&self) {
        self.0.window.set_minimized(true);
    }

    fn zoom(&self) {
        self.0.window.set_maximized(!self.0.window.is_maximized());
    }

    fn toggle_fullscreen(&self) {
        let fullscreen = self
            .0
            .window
            .fullscreen()
            .is_none()
            .then_some(winit::window::Fullscreen::Borderless(None));
        self.0.window.set_fullscreen(fullscreen);
    }

    fn is_fullscreen(&self) -> bool {
        self.0.window.fullscreen().is_some()
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.state.borrow_mut().callbacks.request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.state.borrow_mut().callbacks.input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.state.borrow_mut().callbacks.active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.state.borrow_mut().callbacks.hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.state.borrow_mut().callbacks.resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.state.borrow_mut().callbacks.moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.state.borrow_mut().callbacks.should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0.state.borrow_mut().callbacks.hit_test_window_control = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.state.borrow_mut().callbacks.close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.state.borrow_mut().callbacks.appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.0.state.borrow_mut();
        match state.renderer.draw(scene) {
            Ok(outcome) => {
                // Two skip reasons mean no further frame can help until the window system says
                // otherwise: nothing is visible, or there is no device to draw with. An undamaged
                // frame is deliberately *not* one of them — nothing changed this time, but the
                // next change must still be drawn.
                self.0.frames_suppressed.set(matches!(
                    outcome,
                    FrameOutcome::Skipped(
                        zgui_render::SkipReason::Occluded
                            | zgui_render::SkipReason::DeviceUnavailable
                    )
                ));
            }
            Err(error) => log::error!("gpui_zgui: a frame failed to draw: {error:#}"),
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.0.state.borrow().renderer.supports_subpixel_text()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        let state = self.0.state.borrow();
        let info = state.renderer.gpu().adapter().get_info();
        Some(GpuSpecs {
            is_software_emulated: info.device_type == zgui_render_wgpu::wgpu::DeviceType::Cpu,
            device_name: info.name,
            driver_name: info.driver,
            driver_info: info.driver_info,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn gpu_context(&self) -> Option<Box<dyn std::any::Any>> {
        // zgui opens its own device rather than accepting one, so the direction of this API is
        // inverted here: an embedder takes the renderer's device instead of supplying it. The
        // tuple shape matches what the other Linux backends hand out.
        let state = self.0.state.borrow();
        let gpu = state.renderer.gpu();
        Some(Box::new((gpu.device().clone(), gpu.queue().clone())))
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        let scale = self.scale_factor();
        self.0.window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(
                f32::from(bounds.origin.x) * scale,
                f32::from(bounds.origin.y) * scale,
            ),
            winit::dpi::PhysicalSize::new(
                f32::from(bounds.size.width) * scale,
                f32::from(bounds.size.height) * scale,
            ),
        );
    }
}

/// gpui's cursor vocabulary, in winit's.
///
/// winit has no icon for a few of gpui's styles; each falls back to the closest thing the window
/// system does offer rather than to the default arrow, so the pointer still says something.
pub fn cursor_icon(style: CursorStyle) -> winit::window::Cursor {
    use winit::window::CursorIcon;
    let icon = match style {
        CursorStyle::Arrow => CursorIcon::Default,
        CursorStyle::IBeam => CursorIcon::Text,
        CursorStyle::Crosshair => CursorIcon::Crosshair,
        CursorStyle::ClosedHand => CursorIcon::Grabbing,
        CursorStyle::OpenHand => CursorIcon::Grab,
        CursorStyle::PointingHand => CursorIcon::Pointer,
        CursorStyle::ResizeLeft => CursorIcon::WResize,
        CursorStyle::ResizeRight => CursorIcon::EResize,
        CursorStyle::ResizeLeftRight => CursorIcon::EwResize,
        CursorStyle::ResizeUp => CursorIcon::NResize,
        CursorStyle::ResizeDown => CursorIcon::SResize,
        CursorStyle::ResizeUpDown => CursorIcon::NsResize,
        CursorStyle::ResizeUpLeftDownRight => CursorIcon::NwseResize,
        CursorStyle::ResizeUpRightDownLeft => CursorIcon::NeswResize,
        CursorStyle::ResizeColumn => CursorIcon::ColResize,
        CursorStyle::ResizeRow => CursorIcon::RowResize,
        CursorStyle::IBeamCursorForVerticalLayout => CursorIcon::VerticalText,
        CursorStyle::OperationNotAllowed => CursorIcon::NotAllowed,
        CursorStyle::DragLink => CursorIcon::Alias,
        CursorStyle::DragCopy => CursorIcon::Copy,
        CursorStyle::ContextualMenu => CursorIcon::ContextMenu,
    };
    winit::window::Cursor::Icon(icon)
}
