//! The phase of a display's vblanks, for aligning rate-limited frame slots
//! (macOS).
//!
//! When the frame slots are locked to the display's rate (see
//! `vizia_core::context::frame_slot_rate`), their phase relative to vblank
//! still matters: a schedule whose frames reach the compositor right at a vblank
//! boundary jitters between two vblanks (one shown twice, the next skipped).
//! And the phase drifts: the display's real rate differs from its nominal rate
//! by tens of ppm (59.9533 vs 59.9505 Hz measured on one display, 47 ppm), so a
//! free-running schedule slides one whole vblank every few minutes.
//!
//! The vblank times come from a `CVDisplayLink`, used only to read time: every
//! [`SAMPLE_EVERY`] the link is started, records the host time of a few vblanks
//! and is stopped again. It never drives rendering. Measured (macOS 26, M1,
//! 59.95 Hz display):
//! - A freshly started link is anchored to the hardware: anchors taken 5 s apart
//!   move by the display's real drift (-0.20 to -0.23 ms per 5 s, i.e. 47 ppm
//!   against the nominal rate) within ~0.03 ms of each other.
//! - A link left running is not: its timestamps follow the nominal rate and
//!   jump back to the hardware only after errors of 3-4 ms (0.25 vblank).
//! - Single timestamps can come early by up to ~2 ms, so the anchor is the
//!   latest lattice point over the sample, which skips the early ones. Even so
//!   an anchor is sometimes ~1 ms off, which is why the period is not estimated
//!   from successive anchors (a 3 s baseline gave -377 ppm for a real -47 ppm):
//!   the slots use the nominal period, re-anchored often enough that the drift
//!   stays small.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the vblank phase is re-read. In between, the phase drifts by the
/// display's rate error (47 ppm measured: ~0.5 ms per 10 s, 0.03 vblank at
/// 60 Hz), far below the ~0.3 vblank of margin either side of the target.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// Vblanks recorded per sample.
const SAMPLE_VBLANKS: usize = 6;
/// Give up on a sample that hasn't collected its vblanks by then (display
/// asleep, link not delivering).
const SAMPLE_TIMEOUT: Duration = Duration::from_millis(500);
/// An anchor older than this is not used (e.g. sampling stopped working).
const ANCHOR_MAX_AGE: Duration = Duration::from_secs(30);

#[repr(C)]
#[derive(Clone, Copy)]
struct CVSMPTETime {
    subframes: i16,
    subframe_divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CVTimeStamp {
    version: u32,
    video_time_scale: i32,
    video_time: i64,
    host_time: u64,
    rate_scalar: f64,
    video_refresh_period: i64,
    smpte_time: CVSMPTETime,
    flags: u64,
    reserved: u64,
}

#[repr(C)]
struct CVTime {
    time_value: i64,
    time_scale: i32,
    flags: i32,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

type CVDisplayLinkRef = *mut c_void;
type OutputCallback = extern "C" fn(
    CVDisplayLinkRef,
    *const CVTimeStamp,
    *const CVTimeStamp,
    u64,
    *mut u64,
    *mut c_void,
) -> i32;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(display: u32, link: *mut CVDisplayLinkRef) -> i32;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: CVDisplayLinkRef) -> CVTime;
    fn CVDisplayLinkSetOutputCallback(
        link: CVDisplayLinkRef,
        callback: OutputCallback,
        user_info: *mut c_void,
    ) -> i32;
    fn CVDisplayLinkStart(link: CVDisplayLinkRef) -> i32;
    fn CVDisplayLinkStop(link: CVDisplayLinkRef) -> i32;
    fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

const CV_TIME_IS_INDEFINITE: i32 = 1;

/// Host times (mach_absolute_time) of the vblanks seen during a sample.
type Samples = Mutex<Vec<u64>>;

/// Records the vblank's host time; nothing else runs on the display link's thread.
extern "C" fn record_vblank(
    _link: CVDisplayLinkRef,
    now: *const CVTimeStamp,
    _output_time: *const CVTimeStamp,
    _flags_in: u64,
    _flags_out: *mut u64,
    context: *mut c_void,
) -> i32 {
    // SAFETY: `context` is the `Samples` owned by the `VblankClock`, which stops
    // and releases the link before dropping it; `now` is valid for the call.
    let (samples, host_time) = unsafe { (&*(context as *const Samples), (*now).host_time) };
    if let Ok(mut samples) = samples.try_lock() {
        if samples.len() < SAMPLE_VBLANKS {
            samples.push(host_time);
        }
    }
    0
}

/// Converts mach host time to `Instant` (macOS's `Instant` runs on the same clock).
fn host_to_instant(host: u64) -> Instant {
    let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: plain calls; `tb` is a valid out-parameter.
    let (now_host, now) = unsafe {
        mach_timebase_info(&mut tb);
        (mach_absolute_time(), Instant::now())
    };
    let to_ns = |ticks: u64| (ticks as u128 * tb.numer as u128 / tb.denom.max(1) as u128) as u64;
    if host <= now_host {
        now - Duration::from_nanos(to_ns(now_host - host))
    } else {
        now + Duration::from_nanos(to_ns(host - now_host))
    }
}

/// Vblank phase of one display.
pub(crate) struct VblankClock {
    display: u32,
    link: CVDisplayLinkRef,
    samples: Arc<Samples>,
    /// Nominal refresh period of the display.
    period: Duration,
    sampling_since: Option<Instant>,
    next_sample: Instant,
    /// A recent vblank time and when it was measured.
    anchor: Option<(Instant, Instant)>,
}

impl VblankClock {
    pub(crate) fn new(display: u32) -> Option<Self> {
        let samples = Arc::new(Samples::default());
        // SAFETY: the link is created here and released in `drop`; the
        // callback's context outlives it (see `record_vblank`).
        unsafe {
            let mut link = std::ptr::null_mut();
            if CVDisplayLinkCreateWithCGDisplay(display, &mut link) != 0 || link.is_null() {
                return None;
            }
            let p = CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link);
            if p.flags & CV_TIME_IS_INDEFINITE != 0 || p.time_value <= 0 || p.time_scale <= 0 {
                CVDisplayLinkRelease(link);
                return None;
            }
            let period = Duration::from_secs_f64(p.time_value as f64 / p.time_scale as f64);
            CVDisplayLinkSetOutputCallback(link, record_vblank, Arc::as_ptr(&samples) as *mut _);
            Some(Self {
                display,
                link,
                samples,
                period,
                sampling_since: None,
                next_sample: Instant::now(),
                anchor: None,
            })
        }
    }

    pub(crate) fn display(&self) -> u32 {
        self.display
    }

    /// Starts or finishes a sample when due. Call it from the event loop while
    /// the phase is wanted; it returns when it next needs to run, if sampling.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Instant> {
        match self.sampling_since {
            None if now >= self.next_sample => {
                self.samples.lock().unwrap().clear();
                // SAFETY: valid link (see `new`).
                if unsafe { CVDisplayLinkStart(self.link) } == 0 {
                    self.sampling_since = Some(now);
                    return Some(now + self.period * SAMPLE_VBLANKS as u32);
                }
                self.next_sample = now + SAMPLE_EVERY;
                None
            }
            None => None,
            Some(since) => {
                let done = self.samples.lock().unwrap().len() >= SAMPLE_VBLANKS;
                if !done && now < since + SAMPLE_TIMEOUT {
                    return Some(now + self.period);
                }
                // SAFETY: valid, started link.
                unsafe { CVDisplayLinkStop(self.link) };
                self.sampling_since = None;
                self.next_sample = now + SAMPLE_EVERY;
                let hosts = std::mem::take(&mut *self.samples.lock().unwrap());
                if let Some(anchor) = self.lattice_anchor(&hosts) {
                    self.anchor = Some((anchor, now));
                }
                None
            }
        }
    }

    /// The latest lattice point over the samples: each vblank time minus the
    /// whole periods since the first, maximised (early timestamps are skipped).
    fn lattice_anchor(&self, hosts: &[u64]) -> Option<Instant> {
        let first = hosts.first().map(|&h| host_to_instant(h))?;
        let p = self.period.as_secs_f64();
        hosts
            .iter()
            .map(|&h| {
                let t = host_to_instant(h);
                let k = (t.saturating_duration_since(first).as_secs_f64() / p).round();
                t - self.period.mul_f64(k)
            })
            .max()
    }

    /// Moves `due` to the nearest time that is `phase` (a fraction of the
    /// period) after a vblank, if a recent vblank time is known.
    pub(crate) fn align(&self, due: Instant, phase: f64, now: Instant) -> Instant {
        let Some((anchor, measured)) = self.anchor else { return due };
        if now.saturating_duration_since(measured) > ANCHOR_MAX_AGE {
            return due;
        }
        let p = self.period.as_secs_f64();
        let target = anchor + self.period.mul_f64(phase);
        let offset = if due >= target {
            due.duration_since(target).as_secs_f64()
        } else {
            -target.duration_since(due).as_secs_f64()
        };
        let k = (offset / p).round();
        if k >= 0.0 { target + self.period.mul_f64(k) } else { target - self.period.mul_f64(-k) }
    }
}

impl Drop for VblankClock {
    fn drop(&mut self) {
        // SAFETY: valid link; stopped before release so the callback (whose
        // context is `self.samples`) no longer runs.
        unsafe {
            CVDisplayLinkStop(self.link);
            CVDisplayLinkRelease(self.link);
        }
    }
}
