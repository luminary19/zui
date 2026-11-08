use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use gpui_util::ResultExt;
use parking_lot::{Condvar, Mutex};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo},
    System::Performance::QueryPerformanceFrequency,
};

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz
/// How often a parked `VSyncProvider` thread still wakes to check for GPU device loss.
pub(crate) const PARKED_DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct VSyncProvider {
    interval: Duration,
    f: Box<dyn Fn() -> bool>,
}

impl VSyncProvider {
    pub(crate) fn new() -> Self {
        let interval = get_dwm_interval()
            .context("Failed to get DWM interval")
            .log_err()
            .unwrap_or(DEFAULT_VSYNC_INTERVAL);
        let f = Box::new(|| unsafe { DwmFlush().is_ok() });
        Self { interval, f }
    }

    pub(crate) fn wait_for_vsync(&self) {
        let vsync_start = Instant::now();
        let wait_succeeded = (self.f)();
        let elapsed = vsync_start.elapsed();
        // DwmFlush and DCompositionWaitForCompositorClock returns very early
        // instead of waiting until vblank when the monitor goes to sleep or is
        // unplugged (nothing to present due to desktop occlusion). We use 1ms as
        // a threshold for the duration of the wait functions and fallback to
        // Sleep() if it returns before that. This could happen during normal
        // operation for the first call after the vsync thread becomes non-idle,
        // but it shouldn't happen often.
        if !wait_succeeded || elapsed < VSYNC_INTERVAL_THRESHOLD {
            log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
            std::thread::sleep(self.interval);
        }
    }
}

/// Parks the `VSyncProvider` thread while no window wants frames.
#[derive(Default)]
pub(crate) struct VSyncWaker {
    parked: Mutex<()>,
    wake: Condvar,
}

impl VSyncWaker {
    /// Blocks until `is_demanded` holds, a window requests frames, or `timeout` elapses.
    /// May also return spuriously, so callers re-check demand after it returns.
    pub(crate) fn park(&self, timeout: Duration, is_demanded: impl FnOnce() -> bool) {
        let mut parked = self.parked.lock();
        // Requests set their flag before taking `parked`, so a request either is
        // visible to this check or notifies only once we are waiting.
        if !is_demanded() {
            self.wake.wait_for(&mut parked, timeout);
        }
    }

    fn wake(&self) {
        let _parked = self.parked.lock();
        self.wake.notify_one();
    }
}

/// One window's demand for vsync frames, the Windows counterpart of the display link
/// the macOS backend starts and stops.
pub(crate) struct FrameDemand {
    requested: AtomicBool,
    waker: Arc<VSyncWaker>,
}

impl FrameDemand {
    pub(crate) fn new(waker: Arc<VSyncWaker>) -> Self {
        Self {
            requested: AtomicBool::new(true),
            waker,
        }
    }

    pub(crate) fn request(&self) {
        if !self.requested.swap(true, Ordering::AcqRel) {
            self.waker.wake();
        }
    }

    pub(crate) fn pause(&self) {
        self.requested.store(false, Ordering::Release);
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

fn get_dwm_interval() -> Result<Duration> {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }?;
    // Check for interval values that are impossibly low. A 29 microsecond
    // interval was seen (from a qpcRefreshPeriod of 60).
    let plausible = |interval: &Duration| *interval >= VSYNC_INTERVAL_THRESHOLD;
    retrieve_duration(timing_info.qpcRefreshPeriod, *QPC_TICKS_PER_SECOND)
        .filter(plausible)
        .or_else(|| {
            retrieve_duration(
                timing_info.rateRefresh.uiDenominator as u64,
                timing_info.rateRefresh.uiNumerator as u64,
            )
            .filter(plausible)
        })
        .context("DWM reported no plausible refresh period")
}

/// The duration of `counts` ticks of a clock that ticks `ticks_per_second` times a second,
/// or `None` for a zero rate.
#[inline]
fn retrieve_duration(counts: u64, ticks_per_second: u64) -> Option<Duration> {
    let nanos = (counts as u128 * 1_000_000_000).checked_div(ticks_per_second as u128)?;
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieve_duration_converts_qpc_ticks() {
        // A 60 Hz refresh period on a 10 MHz performance counter.
        assert_eq!(
            retrieve_duration(166_667, 10_000_000),
            Some(Duration::from_nanos(16_666_700))
        );
    }

    #[test]
    fn retrieve_duration_converts_refresh_rate_fractions() {
        // The DWM fallback passes the refresh rate as denominator / numerator.
        assert_eq!(
            retrieve_duration(1, 60),
            Some(Duration::from_nanos(16_666_666))
        );
        assert_eq!(
            retrieve_duration(1001, 60_000),
            Some(Duration::from_nanos(16_683_333))
        );
    }

    #[test]
    fn retrieve_duration_rejects_a_zero_rate() {
        assert_eq!(retrieve_duration(1, 0), None);
    }

    #[test]
    fn park_times_out_while_paused() {
        let demand = FrameDemand::new(Arc::default());
        demand.pause();
        let start = Instant::now();
        demand
            .waker
            .park(Duration::from_millis(50), || demand.is_requested());
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert!(!demand.is_requested());
    }

    #[test]
    fn request_after_pause_wakes_parked_thread() {
        let demand = Arc::new(FrameDemand::new(Arc::default()));
        demand.pause();
        let parked = std::thread::spawn({
            let demand = demand.clone();
            move || {
                let start = Instant::now();
                while !demand.is_requested() {
                    demand
                        .waker
                        .park(Duration::from_secs(30), || demand.is_requested());
                }
                start.elapsed()
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        demand.request();
        assert!(parked.join().unwrap() < Duration::from_secs(30));
    }
}
