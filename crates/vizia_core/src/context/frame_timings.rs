//! Per-phase timings of rendered frames, for profiling overlays and diagnostics.

use web_time::{Duration, Instant};

/// A phase of producing one frame. Used by windowing backends to report how
/// long each phase took via [`BackendContext::record_frame_phase`](crate::backend::BackendContext::record_frame_phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FramePhase {
    /// Dispatching queued events to models and views (including timers).
    Events,
    /// Restyling (selector matching, transitions).
    Style,
    /// Layout, text layout and other visual updates.
    Layout,
    /// Making the window's graphics context current before drawing.
    MakeCurrent,
    /// Recording draw commands for dirty views (the draw-tree traversal).
    Draw,
    /// Flushing recorded GPU work to the graphics driver.
    Flush,
    /// Presenting the frame (e.g. swapping buffers).
    Present,
}

/// Wall-clock durations of each phase of a rendered frame.
///
/// `events`, `style` and `layout` accumulate over all event-loop iterations
/// since the previous frame was presented, so they include work that led to
/// the frame but happened before the redraw was requested.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameTimings {
    pub events: Duration,
    pub style: Duration,
    pub layout: Duration,
    pub make_current: Duration,
    pub draw: Duration,
    pub flush: Duration,
    pub present: Duration,
    /// When the backend finished presenting this frame.
    pub presented_at: Option<Instant>,
    /// Time between this frame's `presented_at` and the previous frame's.
    pub interval: Option<Duration>,
    /// Number of frames presented so far, including this one.
    pub frame_count: u64,
}

impl FrameTimings {
    /// Sum of all phase durations, i.e. CPU-side time spent on the frame on the UI thread.
    pub fn total(&self) -> Duration {
        self.events
            + self.style
            + self.layout
            + self.make_current
            + self.draw
            + self.flush
            + self.present
    }

    pub(crate) fn add(&mut self, phase: FramePhase, duration: Duration) {
        let slot = match phase {
            FramePhase::Events => &mut self.events,
            FramePhase::Style => &mut self.style,
            FramePhase::Layout => &mut self.layout,
            FramePhase::MakeCurrent => &mut self.make_current,
            FramePhase::Draw => &mut self.draw,
            FramePhase::Flush => &mut self.flush,
            FramePhase::Present => &mut self.present,
        };
        *slot += duration;
    }
}

/// Accumulates the in-progress frame and keeps the last completed one, plus
/// the frame-pacing settings shared between the app and the backend.
#[derive(Debug, Default)]
pub(crate) struct FrameTimingState {
    pub(crate) pending: FrameTimings,
    pub(crate) last: FrameTimings,
    /// Upper bound on presented frames per second requested by the app.
    pub(crate) max_frame_rate: Option<f64>,
    /// Refresh rate of the display the window is on, as reported by the backend.
    pub(crate) display_refresh_rate: Option<f64>,
}

impl FrameTimingState {
    pub(crate) fn finish_frame(&mut self, presented_at: Instant) {
        let mut done = std::mem::take(&mut self.pending);
        done.interval =
            self.last.presented_at.map(|prev| presented_at.saturating_duration_since(prev));
        done.presented_at = Some(presented_at);
        done.frame_count = self.last.frame_count + 1;
        self.last = done;
    }

    /// Minimum time between the starts of two frames, if a frame-rate limit is set.
    pub(crate) fn min_frame_interval(&self) -> Option<Duration> {
        self.max_frame_rate
            .filter(|r| r.is_finite() && *r > 0.0)
            .map(|r| Duration::from_secs_f64(1.0 / r))
    }
}
