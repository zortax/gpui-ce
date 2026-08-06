//! Routing winit window events into gpui.
//!
//! The shape of every arm is the same: update the window's remembered pointer or modifier state,
//! build the gpui event, and dispatch it. The state has to be updated first because gpui's events
//! carry the modifiers and position that were in effect *at* the event, and because
//! [`PlatformWindow::mouse_position`](gpui::PlatformWindow::mouse_position) is queried during
//! handling.
//!
//! Nothing here holds a borrow on the window across a dispatch: handling an event routinely calls
//! back into the window it came from.

use gpui::{
    KeyDownEvent, KeyUpEvent, Keystroke, ModifiersChangedEvent, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, Point, ScrollWheelEvent, TouchPhase, px,
};
use winit::event::{Ime, WindowEvent};
use winit::window::WindowId;

use crate::input;
use crate::platform::PlatformState;

pub(crate) fn handle(state: &PlatformState, id: WindowId, event: WindowEvent) {
    let Some(window) = state.window_for(id) else {
        return;
    };

    // Anything at all may have made drawing worthwhile again — being unoccluded arrives as an
    // ordinary event like any other.
    if !matches!(event, WindowEvent::RedrawRequested) {
        window.resume_frames();
    }

    match event {
        WindowEvent::RedrawRequested => {
            window.begin_frame();
            window.request_frame(gpui::RequestFrameOptions::default());
        }

        WindowEvent::Resized(size) => {
            let scale = window.winit().scale_factor() as f32;
            window.surface_resized(size.width, size.height, scale);
            window.resized(
                gpui::Size {
                    width: px(size.width as f32 / scale),
                    height: px(size.height as f32 / scale),
                },
                scale,
            );
            window.winit().request_redraw();
        }

        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            let size = window.winit().inner_size();
            window.surface_resized(size.width, size.height, scale_factor as f32);
            window.resized(
                gpui::Size {
                    width: px(size.width as f32 / scale_factor as f32),
                    height: px(size.height as f32 / scale_factor as f32),
                },
                scale_factor as f32,
            );
            window.winit().request_redraw();
        }

        WindowEvent::CloseRequested => {
            if window.should_close() {
                window.closed();
                state
                    .windows
                    .borrow_mut()
                    .retain(|open| open.winit().id() != id);
            }
        }

        WindowEvent::Destroyed => {
            state
                .windows
                .borrow_mut()
                .retain(|open| open.winit().id() != id);
        }

        WindowEvent::Focused(focused) => window.active_status_changed(focused),

        WindowEvent::ModifiersChanged(modifiers) => {
            let modifiers = input::modifiers(modifiers.state());
            let capslock = input::capslock();
            {
                let mut inner = window.state();
                inner.input.modifiers = modifiers;
                inner.input.capslock = capslock;
            }
            window.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }));
        }

        WindowEvent::CursorMoved { position, .. } => {
            let scale = window.winit().scale_factor() as f32;
            let point = Point {
                x: px(position.x as f32 / scale),
                y: px(position.y as f32 / scale),
            };
            let (modifiers, pressed_button) = {
                let mut inner = window.state();
                inner.input.mouse_position = point;
                (inner.input.modifiers, inner.input.pressed_button)
            };
            window.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                position: point,
                pressed_button,
                modifiers,
            }));
        }

        WindowEvent::CursorEntered { .. } => window.hover_status_changed(true),

        WindowEvent::CursorLeft { .. } => {
            let (position, modifiers) = {
                let inner = window.state();
                (inner.input.mouse_position, inner.input.modifiers)
            };
            window.dispatch_input(PlatformInput::MouseExited(MouseExitEvent {
                position,
                pressed_button: None,
                modifiers,
            }));
            window.hover_status_changed(false);
        }

        WindowEvent::MouseInput {
            state: element_state,
            button,
            ..
        } => {
            let Some(button) = input::mouse_button(button) else {
                return;
            };
            let pressed = input::is_pressed(element_state);
            let (position, modifiers, click_count) = {
                let mut inner = window.state();
                let position = inner.input.mouse_position;
                let modifiers = inner.input.modifiers;
                let count = if pressed {
                    inner.input.pressed_button = Some(button);
                    inner.input.clicks.press(button, position)
                } else {
                    inner.input.pressed_button = None;
                    inner.input.clicks.release()
                };
                (position, modifiers, count)
            };

            let event = if pressed {
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                    first_mouse: false,
                })
            } else {
                PlatformInput::MouseUp(MouseUpEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                })
            };
            window.dispatch_input(event);
        }

        WindowEvent::MouseWheel { delta, phase, .. } => {
            let scale = window.winit().scale_factor() as f32;
            let (position, modifiers) = {
                let inner = window.state();
                (inner.input.mouse_position, inner.input.modifiers)
            };
            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: input::scroll_delta(delta, scale),
                modifiers,
                touch_phase: touch_phase(phase),
            }));
        }

        WindowEvent::KeyboardInput { event, .. } => {
            let modifiers = window.state().input.modifiers;
            let Some(key) = input::key_name(&event.logical_key) else {
                return;
            };
            let keystroke = Keystroke {
                modifiers,
                key,
                key_char: input::key_char(&event.logical_key, modifiers),
            };
            let input = if input::is_pressed(event.state) {
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: event.repeat,
                    prefer_character_input: false,
                })
            } else {
                PlatformInput::KeyUp(KeyUpEvent { keystroke })
            };
            window.dispatch_input(input);
        }

        // Composition. gpui's input handler is modelled on `NSTextInputClient`, which is finer
        // grained than winit's two-event protocol: there is no way to learn the replacement range,
        // so a preedit always replaces the current marked text and a commit always replaces it
        // with final text.
        WindowEvent::Ime(Ime::Preedit(text, cursor)) => {
            let Some(mut handler) = window.state().input_handler.take() else {
                return;
            };
            if text.is_empty() {
                handler.unmark_text();
            } else {
                let selection = cursor.map(|(start, end)| start..end);
                handler.replace_and_mark_text_in_range(None, &text, selection);
            }
            window.state().input_handler = Some(handler);
        }

        WindowEvent::Ime(Ime::Commit(text)) => {
            let Some(mut handler) = window.state().input_handler.take() else {
                return;
            };
            handler.replace_text_in_range(None, &text);
            window.state().input_handler = Some(handler);
        }

        WindowEvent::Moved(_) => {
            let Some(mut moved) = window.state().callbacks.moved.take() else {
                return;
            };
            moved();
            window.state().callbacks.moved = Some(moved);
        }

        _ => {}
    }
}

fn touch_phase(phase: winit::event::TouchPhase) -> TouchPhase {
    match phase {
        winit::event::TouchPhase::Started => TouchPhase::Started,
        winit::event::TouchPhase::Moved => TouchPhase::Moved,
        winit::event::TouchPhase::Ended => TouchPhase::Ended,
        winit::event::TouchPhase::Cancelled => TouchPhase::Cancelled,
    }
}
