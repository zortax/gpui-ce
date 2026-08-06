//! The gpui [`Platform`] implementation, over a winit event loop.
//!
//! # Why there is a raw pointer in here
//!
//! winit 0.30 will only create a window from an [`ActiveEventLoop`], which exists solely for the
//! duration of one handler callback. gpui, meanwhile, calls [`Platform::open_window`] whenever an
//! application asks for a window — from `on_finish_launching`, from a main-thread task, from a
//! menu action. Those are all *inside* a handler callback, but the borrow cannot be threaded
//! through gpui's trait to prove it.
//!
//! So the active loop is parked in a thread-local for exactly as long as a callback is running,
//! and [`with_active_event_loop`] reads it back. The pointer is set and cleared by an RAII guard
//! on entry to every handler method, is only ever read on the same thread that set it, and is
//! null at every point where no callback is running — which is precisely when a window cannot be
//! created anyway, and is reported as an error rather than a crash.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use gpui::{
    AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle, DummyKeyboardMapper,
    ForegroundExecutor, Keymap, Menu, MenuItem, OwnedMenu, PathPromptOptions, Platform,
    PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Task, ThermalState, WindowAppearance, WindowParams,
};
use gpui_wgpu::CosmicTextSystem;
use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use crate::dispatcher::{MainQueue, ZguiDispatcher};
use crate::display::ZguiDisplay;
use crate::keyboard::ZguiKeyboardLayout;
use crate::renderer::ZguiRenderer;
use crate::window::ZguiWindow;

mod events;

/// What the event loop is woken up for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserEvent {
    /// Main-thread runnables are waiting.
    RunMainTasks,
}

thread_local! {
    /// The event loop currently running a callback, or null.
    static ACTIVE_EVENT_LOOP: Cell<*const ActiveEventLoop> = const { Cell::new(std::ptr::null()) };
}

/// Parks the active event loop for the length of a callback.
struct ActiveLoopGuard;

impl ActiveLoopGuard {
    fn enter(event_loop: &ActiveEventLoop) -> Self {
        ACTIVE_EVENT_LOOP.with(|cell| cell.set(event_loop as *const _));
        Self
    }
}

impl Drop for ActiveLoopGuard {
    fn drop(&mut self) {
        ACTIVE_EVENT_LOOP.with(|cell| cell.set(std::ptr::null()));
    }
}

/// Runs `f` with the event loop currently dispatching a callback.
///
/// Fails rather than panics when there is none, because "a window was asked for outside the event
/// loop" is a usage error an embedder can act on.
fn with_active_event_loop<R>(f: impl FnOnce(&ActiveEventLoop) -> R) -> Result<R> {
    let pointer = ACTIVE_EVENT_LOOP.with(|cell| cell.get());
    anyhow::ensure!(
        !pointer.is_null(),
        "gpui_zgui: windows can only be opened from the event loop, \
         which means from inside a gpui callback or a main-thread task"
    );
    // Safety: the pointer was parked by `ActiveLoopGuard` on this thread and is cleared when that
    // guard drops, so it is live for the whole of this call. `ActiveEventLoop` is not `Send`, and
    // the thread-local means no other thread can observe this value.
    Ok(f(unsafe { &*pointer }))
}

/// Callbacks gpui registers on the platform itself.
#[derive(Default)]
struct PlatformCallbacks {
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
    app_menu_action: Option<Box<dyn FnMut(&dyn gpui::Action)>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn gpui::Action) -> bool>>,
}

pub(crate) struct PlatformState {
    background: BackgroundExecutor,
    foreground: ForegroundExecutor,
    text_system: Arc<CosmicTextSystem>,
    main_queue: Arc<Mutex<MainQueue>>,
    event_loop: RefCell<Option<EventLoop<UserEvent>>>,
    pub(crate) windows: RefCell<Vec<ZguiWindow>>,
    displays: RefCell<Vec<Rc<ZguiDisplay>>>,
    callbacks: RefCell<PlatformCallbacks>,
    appearance: Cell<WindowAppearance>,
}

impl PlatformState {
    pub(crate) fn window_for(&self, id: WindowId) -> Option<ZguiWindow> {
        self.windows
            .borrow()
            .iter()
            .find(|window| window.winit().id() == id)
            .cloned()
    }

    /// Runs everything queued for the main thread, reporting whether any of it ran.
    ///
    /// The answer drives redraws: gpui marks a window dirty from inside a task, so a frame is
    /// worth asking for exactly when some task ran.
    pub(crate) fn run_main_tasks(&self) -> bool {
        // Drained into a local first: a runnable is free to queue more main-thread work, and
        // running under the lock would deadlock the moment one did.
        let runnables = self.main_queue.lock().drain();
        let ran = !runnables.is_empty();
        for runnable in runnables {
            runnable.run();
        }
        ran
    }

    /// Asks every window that can still use one for a frame.
    pub(crate) fn request_redraw(&self) {
        for window in self.windows.borrow().iter() {
            if !window.frames_suppressed() {
                window.winit().request_redraw();
            }
        }
    }
}

/// A gpui platform that renders through zgui.
pub struct ZguiPlatform {
    pub(crate) state: Rc<PlatformState>,
}

impl Default for ZguiPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl ZguiPlatform {
    /// A new platform. Must be constructed on the thread that will run the application.
    pub fn new() -> Self {
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .expect("gpui_zgui: the winit event loop could not be created");
        let main_queue = Arc::new(Mutex::new(MainQueue::default()));
        let dispatcher = Arc::new(ZguiDispatcher::new(
            event_loop.create_proxy(),
            main_queue.clone(),
        ));

        Self {
            state: Rc::new(PlatformState {
                background: BackgroundExecutor::new(dispatcher.clone()),
                foreground: ForegroundExecutor::new(dispatcher),
                text_system: Arc::new(CosmicTextSystem::new("sans-serif")),
                main_queue,
                event_loop: RefCell::new(Some(event_loop)),
                windows: RefCell::new(Vec::new()),
                displays: RefCell::new(Vec::new()),
                callbacks: RefCell::new(PlatformCallbacks::default()),
                appearance: Cell::new(WindowAppearance::Dark),
            }),
        }
    }
}

/// The winit application, which owns the platform for the length of the run.
struct App {
    state: Rc<PlatformState>,
    /// Run once, when the event loop first becomes able to create windows.
    on_finish_launching: Option<Box<dyn FnOnce()>>,
    /// Whether something has happened that a frame might need to reflect.
    dirty: bool,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let _guard = ActiveLoopGuard::enter(event_loop);
        // Monitors can only be enumerated from an active loop, and they can change while the
        // application runs, so the list is refreshed here rather than built once.
        *self.state.displays.borrow_mut() = event_loop
            .available_monitors()
            .enumerate()
            .map(|(index, monitor)| Rc::new(ZguiDisplay::new(index, &monitor)))
            .collect();
        if let Some(launch) = self.on_finish_launching.take() {
            launch();
            // The windows it opened have never been drawn, and on Wayland a surface is not even
            // mapped until something is committed to it.
            self.state.run_main_tasks();
            self.state.request_redraw();
            self.dirty = true;
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        let _guard = ActiveLoopGuard::enter(event_loop);
        if cause == StartCause::Poll {
            self.dirty |= self.state.run_main_tasks();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let _guard = ActiveLoopGuard::enter(event_loop);
        match event {
            UserEvent::RunMainTasks => self.dirty |= self.state.run_main_tasks(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let _guard = ActiveLoopGuard::enter(event_loop);
        let redrawn = matches!(event, WindowEvent::RedrawRequested);
        events::handle(&self.state, id, event);
        // Handling an event routinely wakes tasks; running them now rather than waiting for the
        // proxy round-trip keeps a keystroke and the redraw it causes in the same iteration.
        let ran = self.state.run_main_tasks();
        // Any event other than a redraw is a reason to ask for one. gpui marks a window dirty
        // from inside the handler — a keystroke inserting text does not necessarily queue a task
        // — so waiting for a task to run would leave the typed character invisible until
        // something else happened to schedule work. Asking is close to free: gpui decides for
        // itself whether the frame redraws anything, and an unchanged frame is skipped before it
        // is even translated.
        self.dirty = if redrawn { ran } else { true };
        if self.state.windows.borrow().is_empty() {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let _guard = ActiveLoopGuard::enter(event_loop);
        self.dirty |= self.state.run_main_tasks();

        // Ask for a frame only when something has actually happened: an event arrived, or a
        // main-thread task ran (which is how gpui signals it wants one, animations included).
        // A frame that was merely *presented* is not a reason to draw another; believing it was
        // is an unbounded redraw loop that re-renders the whole element tree as fast as the
        // machine allows.
        if self.dirty {
            self.state.request_redraw();
        }

        // Always `Wait`. `request_redraw` is what schedules the frame; polling on top of it just
        // spins the loop between frames without producing any.
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

impl Platform for ZguiPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.state.background.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.state.foreground.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.state.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let Some(event_loop) = self.state.event_loop.borrow_mut().take() else {
            panic!("gpui_zgui: the platform was already run");
        };
        let mut app = App {
            state: self.state.clone(),
            on_finish_launching: Some(on_finish_launching),
            dirty: true,
        };
        if let Err(error) = event_loop.run_app(&mut app) {
            log::error!("gpui_zgui: the event loop stopped: {error}");
        }
        let mut callbacks = self.state.callbacks.borrow_mut();
        if let Some(quit) = callbacks.quit.as_mut() {
            quit();
        }
    }

    fn quit(&self) {
        self.state.windows.borrow_mut().clear();
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        log::warn!("gpui_zgui: restart is not implemented");
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        if let Some(window) = self.state.windows.borrow().first() {
            window.winit().focus_window();
        }
    }

    fn hide(&self) {}
    fn hide_other_apps(&self) {}
    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        self.state
            .displays
            .borrow()
            .iter()
            .map(|display| display.clone() as Rc<dyn PlatformDisplay>)
            .collect()
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.state
            .displays
            .borrow()
            .first()
            .map(|display| display.clone() as Rc<dyn PlatformDisplay>)
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        None
    }

    fn open_window(
        &self,
        _handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let state = self.state.clone();
        with_active_event_loop(move |event_loop| {
            let scale = event_loop
                .primary_monitor()
                .map_or(1.0, |monitor| monitor.scale_factor());
            let size = winit::dpi::LogicalSize::new(
                f32::from(options.bounds.size.width),
                f32::from(options.bounds.size.height),
            );
            let mut attributes = winit::window::Window::default_attributes()
                .with_inner_size(size)
                .with_resizable(options.is_resizable)
                .with_visible(options.show);
            if let Some(titlebar) = options.titlebar.as_ref()
                && let Some(title) = titlebar.title.as_ref()
            {
                attributes = attributes.with_title(title.to_string());
            }
            if let Some(min) = options.window_min_size {
                attributes = attributes.with_min_inner_size(winit::dpi::LogicalSize::new(
                    f32::from(min.width),
                    f32::from(min.height),
                ));
            }

            let window = Arc::new(
                event_loop
                    .create_window(attributes)
                    .context("creating a winit window")?,
            );
            let physical = window.inner_size();
            // The surface must be created from the very instance the device will be opened on, so
            // the builder is made here and handed straight to the renderer. That means one wgpu
            // instance and one device per window: zgui's builder opens its own device and offers
            // no way to share one, which is a real cost for a multi-window application.
            let builder = zgui_render_wgpu::Builder::new();
            let surface = builder
                .instance()
                .create_surface(window.clone())
                .context("creating a wgpu surface for the window")?;
            let renderer = ZguiRenderer::for_surface(
                builder,
                surface,
                zgui_geom::Size::new(physical.width as i32, physical.height as i32),
                window.scale_factor() as f32,
                true,
            )?;

            let display = state
                .displays
                .borrow()
                .first()
                .cloned()
                .unwrap_or_else(|| Rc::new(ZguiDisplay::placeholder(scale as f32)));
            let platform_window =
                ZguiWindow::new(window, renderer, display, state.appearance.get());
            state.windows.borrow_mut().push(platform_window.clone());
            Ok(Box::new(platform_window) as Box<dyn PlatformWindow>)
        })?
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.state.appearance.get()
    }

    fn open_url(&self, _url: &str) {}
    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.state.callbacks.borrow_mut().open_urls = Some(callback);
    }
    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "gpui_zgui: registering a url scheme is not implemented"
        )))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(None)).ok();
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(None)).ok();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }
    fn reveal_path(&self, _path: &Path) {}
    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.borrow_mut().quit = Some(callback);
    }
    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.borrow_mut().reopen = Some(callback);
    }
    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}
    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        None
    }
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}
    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn gpui::Action)>) {
        self.state.callbacks.borrow_mut().app_menu_action = Some(callback);
    }
    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.borrow_mut().will_open_app_menu = Some(callback);
    }
    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn gpui::Action) -> bool>) {
        self.state.callbacks.borrow_mut().validate_app_menu_command = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }
    fn on_thermal_state_change(&self, _callback: Box<dyn FnMut()>) {}

    fn app_path(&self) -> Result<PathBuf> {
        std::env::current_exe().context("reading the running executable's path")
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        let mut path = std::env::current_exe().context("reading the running executable's path")?;
        path.pop();
        path.push(name);
        Ok(path)
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let icon = crate::window::cursor_icon(style);
        for window in self.state.windows.borrow().iter() {
            window.winit().set_cursor(icon.clone());
        }
    }

    fn hide_cursor_until_mouse_moves(&self) {}

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        None
    }
    fn write_to_clipboard(&self, _item: ClipboardItem) {}

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        None
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, _item: ClipboardItem) {}

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "gpui_zgui: credential storage is not implemented"
        )))
    }
    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }
    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "gpui_zgui: credential storage is not implemented"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(ZguiKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.state.callbacks.borrow_mut().keyboard_layout_change = Some(callback);
    }
}
