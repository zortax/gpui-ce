//! Per-frame measurements, for answering "is any of this actually worth it".
//!
//! The claim this backend makes is that redrawing only what changed is cheaper than redrawing
//! everything. That claim has two halves, and they pull in opposite directions: the GPU does less
//! because passes are scissored, and the CPU does *more* because the scene has to be compared
//! against the last one and translated into a second display list. Neither half is worth
//! asserting without numbers, so this collects them.
//!
//! Enabled with `GPUI_ZGUI_STATS=1`. A summary is printed every [`REPORT_EVERY`] drawn frames and
//! once more when the renderer is dropped, so a short interactive session produces a report
//! without needing to be shut down cleanly.

use std::time::{Duration, Instant};

/// How many drawn frames between reports.
const REPORT_EVERY: u32 = 30;

/// What one frame cost, and how much of the surface it touched.
#[derive(Clone, Copy, Default)]
pub struct Frame {
    /// Deriving damage by comparing against the previous frame.
    pub compare: Duration,
    /// Rewriting the gpui scene as a zgui one.
    pub translate: Duration,
    /// Sorting into batches and planning passes.
    pub finish: Duration,
    /// Encoding and submitting.
    pub submit: Duration,
    /// Pixels the renderer was asked to redraw.
    pub damaged: u64,
    /// Pixels the surface holds.
    pub surface: u64,
    /// Primitives the gpui scene held.
    pub primitives: usize,
}

impl Frame {
    /// Everything this backend spent on the frame.
    fn total(&self) -> Duration {
        self.compare + self.translate + self.finish + self.submit
    }
}

/// Running totals over a session.
pub struct Stats {
    enabled: bool,
    frames: u32,
    /// Frames that were skipped before being translated, because nothing had changed.
    skipped: u32,
    since_report: u32,
    compare: Duration,
    translate: Duration,
    finish: Duration,
    submit: Duration,
    damaged: u64,
    surface: u64,
    primitives: u64,
    /// Frames the comparison gave up on and redrew whole.
    full: u32,
    started: Option<Instant>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            enabled: matches!(std::env::var("GPUI_ZGUI_STATS").as_deref(), Ok("1")),
            frames: 0,
            skipped: 0,
            since_report: 0,
            compare: Duration::ZERO,
            translate: Duration::ZERO,
            finish: Duration::ZERO,
            submit: Duration::ZERO,
            damaged: 0,
            surface: 0,
            primitives: 0,
            full: 0,
            started: None,
        }
    }
}

impl Stats {
    /// Whether measurements are being collected at all.
    ///
    /// Checked before timing anything, so a build with stats off pays only this.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Records a frame that was skipped because nothing had changed.
    pub fn skipped(&mut self) {
        if !self.enabled {
            return;
        }
        self.started.get_or_insert_with(Instant::now);
        self.skipped += 1;
        self.tick();
    }

    /// Records a frame that was drawn.
    pub fn drawn(&mut self, frame: Frame, was_full: bool) {
        if !self.enabled {
            return;
        }
        self.started.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.compare += frame.compare;
        self.translate += frame.translate;
        self.finish += frame.finish;
        self.submit += frame.submit;
        self.damaged += frame.damaged;
        self.surface += frame.surface;
        self.primitives += frame.primitives as u64;
        if was_full {
            self.full += 1;
        }
        self.tick();
    }

    fn tick(&mut self) {
        self.since_report += 1;
        if self.since_report >= REPORT_EVERY {
            self.report();
            self.since_report = 0;
        }
    }

    /// Prints what has been measured so far.
    pub fn report(&self) {
        if !self.enabled || (self.frames == 0 && self.skipped == 0) {
            return;
        }
        let considered = self.frames + self.skipped;
        let per = |total: Duration| total.as_secs_f64() * 1000.0 / self.frames.max(1) as f64;
        // The headline: what fraction of the surface was actually redrawn. A backend that
        // silently degraded to full damage would sit at 100% here and every other number would
        // still look reasonable.
        let redrawn = if self.surface > 0 {
            self.damaged as f64 / self.surface as f64 * 100.0
        } else {
            0.0
        };
        let elapsed = self
            .started
            .map(|at| at.elapsed().as_secs_f64())
            .unwrap_or(0.0);

        eprintln!(
            "gpui_zgui stats: {considered} frames considered in {elapsed:.1}s \
             ({} drawn, {} skipped unchanged, {} redrawn whole)\n  \
             surface redrawn: {redrawn:.1}%   primitives/frame: {}\n  \
             per drawn frame: compare {:.3}ms  translate {:.3}ms  finish {:.3}ms  \
             submit {:.3}ms  = {:.3}ms",
            self.frames,
            self.skipped,
            self.full,
            self.primitives / self.frames.max(1) as u64,
            per(self.compare),
            per(self.translate),
            per(self.finish),
            per(self.submit),
            per(self.compare + self.translate + self.finish + self.submit),
        );
    }
}

impl Drop for Stats {
    fn drop(&mut self) {
        self.report();
    }
}

/// Times `f`, but only when stats are on.
pub fn time<R>(enabled: bool, into: &mut Duration, f: impl FnOnce() -> R) -> R {
    if !enabled {
        return f();
    }
    let at = Instant::now();
    let result = f();
    *into += at.elapsed();
    result
}

impl Frame {
    /// A one-line description, for `GPUI_ZGUI_STATS=1` traces of single frames.
    #[allow(dead_code)]
    pub fn line(&self) -> String {
        format!(
            "frame: {} prims, {:.1}% redrawn, {:.3}ms",
            self.primitives,
            if self.surface > 0 {
                self.damaged as f64 / self.surface as f64 * 100.0
            } else {
                0.0
            },
            self.total().as_secs_f64() * 1000.0,
        )
    }
}
