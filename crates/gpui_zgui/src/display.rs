//! Monitors, as gpui sees them.

use anyhow::Result;
use gpui::{Bounds, DisplayId, PlatformDisplay, Pixels, Point, Size, px};
use uuid::Uuid;
use winit::monitor::MonitorHandle;

/// One monitor.
#[derive(Debug, Clone)]
pub struct ZguiDisplay {
    id: DisplayId,
    bounds: Bounds<Pixels>,
    /// Derived from the monitor's name, so it survives a restart as long as the name does.
    ///
    /// winit exposes no stable hardware identifier, so this is the best available answer. A
    /// monitor with no name at all falls back to its enumeration index, which does not survive
    /// replugging — that is a real limitation of this backend rather than something to paper over.
    uuid: Uuid,
}

/// The namespace display uuids are derived in, so they cannot collide with another scheme's.
const DISPLAY_NAMESPACE: Uuid = Uuid::from_bytes([
    0x9a, 0x1d, 0x4f, 0x2b, 0x7c, 0x36, 0x4e, 0x88, 0xb0, 0x51, 0x3d, 0x7a, 0x6c, 0x94, 0x2e, 0x11,
]);

impl ZguiDisplay {
    /// A display for `monitor`, numbered `index` in the platform's enumeration order.
    pub fn new(index: usize, monitor: &MonitorHandle) -> Self {
        let position = monitor.position();
        let size = monitor.size();
        let scale = monitor.scale_factor() as f32;
        let name = monitor.name().unwrap_or_else(|| format!("display-{index}"));

        Self {
            id: DisplayId::new(index as u64),
            // gpui works in logical pixels; winit reports monitor geometry in physical ones.
            bounds: Bounds {
                origin: Point {
                    x: px(position.x as f32 / scale),
                    y: px(position.y as f32 / scale),
                },
                size: Size {
                    width: px(size.width as f32 / scale),
                    height: px(size.height as f32 / scale),
                },
            },
            uuid: Uuid::new_v5(&DISPLAY_NAMESPACE, name.as_bytes()),
        }
    }

    /// A stand-in for when the window system enumerated no monitors at all.
    ///
    /// Headless sessions and some remote compositors report none, and gpui still wants somewhere
    /// to place a window rather than a failure.
    pub fn placeholder(scale: f32) -> Self {
        let _ = scale;
        Self {
            id: DisplayId::new(0),
            bounds: Bounds {
                origin: Point {
                    x: px(0.0),
                    y: px(0.0),
                },
                size: Size {
                    width: px(1920.0),
                    height: px(1080.0),
                },
            },
            uuid: Uuid::new_v5(&DISPLAY_NAMESPACE, b"placeholder"),
        }
    }
}

impl PlatformDisplay for ZguiDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(self.uuid)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}
