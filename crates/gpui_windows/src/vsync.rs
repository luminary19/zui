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
/// Waits between failed GPU device-loss recoveries: doubled after each failure, up to the max.
const MIN_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(30);

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

/// Paces GPU device-loss recovery on the `VSyncProvider` thread. A requested recovery runs at
/// once; one that fails stays pending and is retried with a growing delay, so a recovery that
/// keeps failing neither stops the thread nor rebuilds every window on every tick.
#[derive(Default)]
pub(crate) struct DeviceRecoveryRetry {
    pending: bool,
    failures: u32,
    next_attempt: Option<Instant>,
}

impl DeviceRecoveryRetry {
    pub(crate) fn request(&mut self) {
        self.pending = true;
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.pending && self.next_attempt.is_none_or(|next| now >= next)
    }

    pub(crate) fn finish(&mut self, recovered: bool, now: Instant) {
        if recovered {
            *self = Self::default();
            return;
        }
        let delay = MIN_RECOVERY_RETRY_DELAY
            .saturating_mul(1 << self.failures.min(5))
            .min(MAX_RECOVERY_RETRY_DELAY);
        self.failures += 1;
        self.next_attempt = Some(now + delay);
    }
}

fn get_dwm_interval() -> Result<Duration> {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }?;
    refresh_period(
        timing_info.qpcRefreshPeriod,
        *QPC_TICKS_PER_SECOND,
        timing_info.rateRefresh.uiNumerator as u64,
        timing_info.rateRefresh.uiDenominator as u64,
    )
    .context("DWM reported no plausible refresh period")
}

/// The refresh period from the QPC tick count, or from the refresh rate when that count is
/// implausible.
fn refresh_period(
    qpc_period: u64,
    qpc_ticks_per_second: u64,
    rate_numerator: u64,
    rate_denominator: u64,
) -> Option<Duration> {
    // Check for interval values that are impossibly low. A 29 microsecond
    // interval was seen (from a qpcRefreshPeriod of 60).
    let plausible = |interval: &Duration| *interval >= VSYNC_INTERVAL_THRESHOLD;
    retrieve_duration(qpc_period, qpc_ticks_per_second)
        .filter(plausible)
        .or_else(|| retrieve_duration(rate_denominator, rate_numerator).filter(plausible))
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
    fn refresh_period_prefers_a_plausible_qpc_period() {
        // The refresh rate alone would give 16_666_666 ns.
        assert_eq!(
            refresh_period(166_667, 10_000_000, 60, 1),
            Some(Duration::from_nanos(16_666_700))
        );
    }

    #[test]
    fn refresh_period_falls_back_to_the_refresh_rate() {
        // The implausible period seen in the wild, with a 60 Hz refresh rate.
        assert_eq!(
            refresh_period(60, 10_000_000, 60, 1),
            Some(Duration::from_nanos(16_666_666))
        );
    }

    #[test]
    fn refresh_period_rejects_an_implausible_fallback() {
        assert_eq!(refresh_period(60, 10_000_000, 1, 0), None);
        assert_eq!(refresh_period(60, 10_000_000, 1_000_000, 1), None);
    }

    #[test]
    fn recovery_runs_once_requested() {
        let now = Instant::now();
        let mut recovery = DeviceRecoveryRetry::default();
        assert!(!recovery.is_due(now));
        recovery.request();
        assert!(recovery.is_due(now));
        recovery.finish(true, now);
        assert!(!recovery.is_due(now + MAX_RECOVERY_RETRY_DELAY));
    }

    #[test]
    fn failed_recovery_is_retried_with_a_growing_capped_delay() {
        let mut now = Instant::now();
        let mut recovery = DeviceRecoveryRetry::default();
        recovery.request();
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            assert!(recovery.is_due(now));
            recovery.finish(false, now);
            // Still pending, and a new device loss does not skip the wait.
            recovery.request();
            let delay = Duration::from_secs(expected);
            assert!(!recovery.is_due(now + delay - Duration::from_millis(1)));
            now += delay;
        }
        assert!(recovery.is_due(now));
        recovery.finish(true, now);
        recovery.request();
        assert!(recovery.is_due(now), "a success resets the backoff");
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
