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

    /// Time between two frame slots, if a frame-rate limit is set. The slot rate
    /// is [`frame_slot_rate`] of the limit and the display's refresh rate.
    pub(crate) fn min_frame_interval(&self) -> Option<Duration> {
        self.max_frame_rate.filter(|r| r.is_finite() && *r > 0.0).map(|cap| {
            Duration::from_secs_f64(1.0 / frame_slot_rate(cap, self.display_refresh_rate))
        })
    }

    /// See [`frame_slot_vblanks`]; `None` without a frame-rate limit.
    pub(crate) fn frame_slot_vblanks(&self) -> Option<u32> {
        frame_slot_vblanks(self.max_frame_rate?, self.display_refresh_rate)
    }
}

/// How far (relative) `refresh / cap` may be from a whole number `n` for the
/// slots to lock to `refresh / n`. Real displays run a little off their nominal
/// rate (a "60 Hz" mode is often 59.94 or 59.951 Hz); 2% covers that with room,
/// while ratios that are really fractional (320 / 60 = 5.33, 144 / 60 = 2.4) are
/// further away than that.
pub const FRAME_SLOT_RATE_TOLERANCE: f64 = 0.02;

/// The rate at which a backend should schedule frames for a frame-rate limit
/// `cap` on a display refreshing at `refresh` Hz.
///
/// Slots at exactly `cap` beat against a display whose real rate is close to a
/// multiple of it: a 60.000 Hz schedule on a 59.951 Hz display lands two frames
/// in one vblank (and none in the next) for seconds at a time, every ~20 s. So:
/// - `refresh <= cap` (within the tolerance): one frame per vblank, `refresh`.
/// - `refresh / cap` within [`FRAME_SLOT_RATE_TOLERANCE`] of a whole number
///   `n > 1`: every n-th vblank, `refresh / n` (119.88 Hz, cap 60: 59.94).
/// - otherwise the cap itself; its fixed-rate slots then spread the frames
///   over the vblanks in the best cadence available (320 Hz, cap 60: 5/5/6).
///
/// The result may exceed `cap` by up to the tolerance (a 60.5 Hz display with a
/// 60 cap paces at 60.5); it never exceeds the display's rate.
pub fn frame_slot_rate(cap: f64, refresh: Option<f64>) -> f64 {
    match (refresh, frame_slot_vblanks(cap, refresh)) {
        (Some(refresh), Some(n)) => refresh / n as f64,
        _ => cap,
    }
}

/// The `n` of [`frame_slot_rate`]: frames are locked to every n-th vblank, or
/// `None` when the slots run at the cap itself (or there is no valid rate).
pub fn frame_slot_vblanks(cap: f64, refresh: Option<f64>) -> Option<u32> {
    let refresh = refresh.filter(|r| r.is_finite() && *r > 0.0)?;
    if !(cap.is_finite() && cap > 0.0) {
        return None;
    }
    let ratio = refresh / cap;
    if ratio <= 1.0 + FRAME_SLOT_RATE_TOLERANCE {
        return Some(1);
    }
    let n = ratio.round();
    ((ratio / n - 1.0).abs() <= FRAME_SLOT_RATE_TOLERANCE).then_some(n as u32)
}

#[cfg(test)]
mod tests {
    use super::{frame_slot_rate, frame_slot_vblanks};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn slot_rate_locks_to_near_multiples_of_the_cap() {
        assert!(close(frame_slot_rate(60.0, Some(59.951)), 59.951));
        assert!(close(frame_slot_rate(60.0, Some(119.88)), 59.94));
        assert!(close(frame_slot_rate(120.0, Some(59.951)), 59.951));
        assert!(close(frame_slot_rate(60.0, Some(240.0)), 60.0));
        assert!(close(frame_slot_rate(60.0, Some(50.0)), 50.0));
    }

    #[test]
    fn slot_rate_keeps_the_cap_for_fractional_ratios() {
        assert_eq!(frame_slot_rate(60.0, Some(320.0)), 60.0);
        assert_eq!(frame_slot_rate(120.0, Some(320.0)), 120.0);
        assert_eq!(frame_slot_rate(60.0, Some(144.0)), 60.0);
        assert_eq!(frame_slot_rate(60.0, Some(75.0)), 60.0);
        assert_eq!(frame_slot_rate(60.0, None), 60.0);
        assert_eq!(frame_slot_rate(60.0, Some(0.0)), 60.0);
    }

    #[test]
    fn slot_vblanks() {
        assert_eq!(frame_slot_vblanks(60.0, Some(59.951)), Some(1));
        assert_eq!(frame_slot_vblanks(60.0, Some(119.88)), Some(2));
        assert_eq!(frame_slot_vblanks(120.0, Some(59.951)), Some(1));
        assert_eq!(frame_slot_vblanks(60.0, Some(320.0)), None);
        assert_eq!(frame_slot_vblanks(60.0, None), None);
    }
}
