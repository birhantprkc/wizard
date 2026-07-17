//! Circuit breaker over the model endpoint, consulted by the streaming retry
//! loop ([`super::Agent::stream_completion_with_retry`]). This is distinct
//! from the tool-failure breakers in [`crate::dispatch`]: this one watches the
//! *provider*, not tool execution. Its job is to stop the retry loop from
//! hammering — or, in continuous mode, retrying *forever* — a provider that is
//! down, and to recover on its own once the provider comes back.
//!
//! The design is adapted from grok-build's `xai-circuit-breaker` (Apache-2.0):
//! the tri-state machine (Closed → Open → HalfOpen), the single half-open
//! recovery probe, and the injectable [`Clock`] for deterministic tests are
//! its shape. The trip *condition* is specialized to this call site. The retry
//! loop records an outcome only when a call fails (a success returns at once),
//! and those failures are spaced by exponential backoff, so an error-*rate*
//! over a sliding time window is trivially ~1.0 during any outage while a
//! short window would risk evicting failures faster than backoff produces
//! them. A consecutive-failure count is therefore the meaningful signal here.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Trip after this many consecutive transient failures. Set above the
/// interactive per-turn retry budget (7 attempts) so a single interactive turn
/// keeps its current behavior — surfacing the provider's own error — and the
/// breaker only opens across a sustained outage that spans turns (or the
/// unbounded retries of a continuous run).
const TRIP_THRESHOLD: u32 = 8;

/// How long the breaker stays open before admitting one recovery probe.
const OPEN_DURATION: Duration = Duration::from_secs(30);

/// Injectable time source so the cooldown transition is testable without
/// sleeping.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// The real wall clock.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Returned by [`LlmBreaker::check`] while the breaker is open: the call must
/// not be made, and `retry_after` is how long until a probe would be admitted.
#[derive(Debug, Clone, Copy)]
pub struct BreakerOpen {
    pub retry_after: Duration,
}

/// The outcome of one model call, fed back to the breaker.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
}

/// Turn-level signal that the endpoint breaker is open. The agent loop maps it
/// to [`super::DoneReason::CircuitBreaker`] — a clean, rolled-back cycle end —
/// rather than a hard error.
#[derive(Debug, thiserror::Error)]
#[error("LLM circuit breaker open; provider unavailable (retry in {}s)", retry_after.as_secs())]
pub struct LlmBreakerOpen {
    pub retry_after: Duration,
}

/// A cheap-to-clone handle on a circuit breaker. All methods take `&self`
/// (state lives behind a mutex) so it sits behind the agent's shared,
/// non-`mut` `stream_completion_with_retry`.
#[derive(Clone)]
pub struct LlmBreaker {
    inner: Arc<Inner>,
}

struct Inner {
    threshold: u32,
    open_duration: Duration,
    clock: Arc<dyn Clock>,
    machine: Mutex<Machine>,
}

struct Machine {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: Instant,
}

impl LlmBreaker {
    /// Breaker with the default threshold and cooldown on the wall clock.
    pub fn new() -> Self {
        Self::with_clock(TRIP_THRESHOLD, OPEN_DURATION, Arc::new(SystemClock))
    }

    fn with_clock(threshold: u32, open_duration: Duration, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            inner: Arc::new(Inner {
                threshold,
                open_duration,
                clock,
                machine: Mutex::new(Machine {
                    state: BreakerState::Closed,
                    consecutive_failures: 0,
                    opened_at: now,
                }),
            }),
        }
    }

    /// The configured cooldown — the full `retry_after` right after a trip.
    pub fn cooldown(&self) -> Duration {
        self.inner.open_duration
    }

    /// Consult before dialing the provider. `Ok` when a call may proceed
    /// (closed, or open past its cooldown → admits one half-open probe);
    /// `Err` while open, carrying the remaining cooldown.
    pub fn check(&self) -> Result<(), BreakerOpen> {
        let now = self.inner.clock.now();
        let mut m = self.lock();
        match m.state {
            BreakerState::Closed | BreakerState::HalfOpen => Ok(()),
            BreakerState::Open => {
                let elapsed = now.saturating_duration_since(m.opened_at);
                if elapsed >= self.inner.open_duration {
                    m.state = BreakerState::HalfOpen; // admit a single probe
                    Ok(())
                } else {
                    Err(BreakerOpen {
                        retry_after: self.inner.open_duration - elapsed,
                    })
                }
            }
        }
    }

    /// Feed back the outcome of a call.
    pub fn record(&self, outcome: Outcome) {
        let now = self.inner.clock.now();
        let mut m = self.lock();
        match (m.state, outcome) {
            // A half-open probe decides recovery.
            (BreakerState::HalfOpen, Outcome::Success) => {
                m.state = BreakerState::Closed;
                m.consecutive_failures = 0;
            }
            (BreakerState::HalfOpen, Outcome::Failure) => {
                m.state = BreakerState::Open;
                m.opened_at = now;
            }
            // Any success clears the failure streak.
            (BreakerState::Closed, Outcome::Success) => {
                m.consecutive_failures = 0;
            }
            (BreakerState::Closed, Outcome::Failure) => {
                m.consecutive_failures += 1;
                if m.consecutive_failures >= self.inner.threshold {
                    m.state = BreakerState::Open;
                    m.opened_at = now;
                }
            }
            // Calls are gated by `check`, so an outcome while open is unusual;
            // ignore it rather than let it disturb `opened_at`.
            (BreakerState::Open, _) => {}
        }
    }

    /// True once the breaker has tripped. Lets the retry loop break the moment
    /// a failure opens it, without first sleeping a full backoff.
    pub fn is_open(&self) -> bool {
        self.lock().state == BreakerState::Open
    }

    pub fn state(&self) -> BreakerState {
        self.lock().state
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Machine> {
        // A poisoned lock only means a prior holder panicked mid-update; the
        // invariants here are plain counters, so recover and carry on rather
        // than propagate the panic into the agent loop.
        self.inner.machine.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for LlmBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockClock {
        now: Mutex<Instant>,
    }
    impl MockClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Instant::now()),
            })
        }
        fn advance(&self, by: Duration) {
            let mut n = self.now.lock().unwrap();
            *n = n.checked_add(by).expect("mock clock overflow");
        }
    }
    impl Clock for MockClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    fn breaker(threshold: u32, open: Duration) -> (LlmBreaker, Arc<MockClock>) {
        let clock = MockClock::new();
        (
            LlmBreaker::with_clock(threshold, open, clock.clone()),
            clock,
        )
    }

    #[test]
    fn closed_until_threshold_then_opens() {
        let (cb, _clock) = breaker(3, Duration::from_secs(30));
        assert_eq!(cb.state(), BreakerState::Closed);
        cb.record(Outcome::Failure);
        cb.record(Outcome::Failure);
        assert!(cb.check().is_ok(), "2 < 3 stays closed");
        cb.record(Outcome::Failure);
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(cb.check().is_err(), "open trips a fast fail");
    }

    #[test]
    fn success_resets_the_streak() {
        let (cb, _clock) = breaker(3, Duration::from_secs(30));
        cb.record(Outcome::Failure);
        cb.record(Outcome::Failure);
        cb.record(Outcome::Success); // clears the streak
        cb.record(Outcome::Failure);
        cb.record(Outcome::Failure);
        assert_eq!(
            cb.state(),
            BreakerState::Closed,
            "only 2 failures since reset"
        );
    }

    #[test]
    fn open_admits_probe_after_cooldown() {
        let (cb, clock) = breaker(1, Duration::from_millis(50));
        cb.record(Outcome::Failure); // threshold 1 → open
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(cb.check().is_err(), "within cooldown, still open");
        clock.advance(Duration::from_millis(60));
        assert!(cb.check().is_ok(), "cooldown elapsed → probe admitted");
        assert_eq!(cb.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_success_closes() {
        let (cb, clock) = breaker(1, Duration::from_millis(50));
        cb.record(Outcome::Failure);
        clock.advance(Duration::from_millis(60));
        assert!(cb.check().is_ok()); // → half-open
        cb.record(Outcome::Success);
        assert_eq!(cb.state(), BreakerState::Closed);
        assert!(cb.check().is_ok());
    }

    #[test]
    fn half_open_failure_reopens() {
        let (cb, clock) = breaker(1, Duration::from_millis(50));
        cb.record(Outcome::Failure);
        clock.advance(Duration::from_millis(60));
        assert!(cb.check().is_ok()); // → half-open
        cb.record(Outcome::Failure);
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(
            cb.check().is_err(),
            "the cooldown restarts on a failed probe"
        );
    }

    #[test]
    fn outcomes_recorded_while_open_do_not_disturb_the_cooldown() {
        let (cb, clock) = breaker(1, Duration::from_millis(50));
        cb.record(Outcome::Failure); // trips at t0
        clock.advance(Duration::from_millis(30));
        // Stray outcomes while open (calls not gated through `check`) must
        // neither close the breaker nor restart its cooldown.
        cb.record(Outcome::Failure);
        cb.record(Outcome::Success);
        assert_eq!(cb.state(), BreakerState::Open);
        clock.advance(Duration::from_millis(25)); // t0 + 55ms
        assert!(
            cb.check().is_ok(),
            "the cooldown still runs from the original trip"
        );
    }
}
