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

/// How long the breaker stays open after its *first* trip before admitting one
/// recovery probe.
const OPEN_DURATION: Duration = Duration::from_secs(30);

/// Ceiling on the escalated cooldown (see [`Machine::trips`]). Long enough that
/// a provider in a multi-hour outage is dialed a handful of times an hour
/// instead of a hundred; short enough that a run waiting one out comes back
/// within a quarter hour of the provider doing so.
const MAX_OPEN_DURATION: Duration = Duration::from_secs(15 * 60);

/// Injectable time source so the cooldown transition is testable without
/// sleeping.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// The real wall clock.
///
/// Read through `tokio::time` rather than `std::time` so the breaker moves with
/// whatever clock the runtime is on. In production the two are the same
/// instant. Under `#[tokio::test(start_paused = true)]` they are not: the
/// runtime's clock jumps forward whenever every task is parked on a timer, and
/// a breaker reading `std::time::Instant::now()` would sit at t0 while the code
/// under test believed an hour had passed — so the one caller that *sleeps* a
/// cooldown ([`super::retry::Ladder::climb`] waiting out an outage) could not be
/// tested at all without really sleeping it.
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
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
    max_open_duration: Duration,
    clock: Arc<dyn Clock>,
    machine: Mutex<Machine>,
}

struct Machine {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: Instant,
    /// Trips since the last time a probe actually closed the breaker.
    ///
    /// A flat cooldown assumes every outage is the same length, and the one
    /// caller that matters most — a continuous run, which waits an open
    /// breaker out instead of dying on it — turns that assumption into a
    /// request every 30 seconds for as long as the provider is down. Doubling
    /// the cooldown per trip means a blip still costs one cooldown while a
    /// multi-hour outage settles at [`MAX_OPEN_DURATION`]. It counts *trips*
    /// rather than failures because a failed half-open probe is the strongest
    /// evidence there is that the last cooldown was too short: the endpoint
    /// was given a full rest and still could not answer one call.
    trips: u32,
}

impl Machine {
    /// Reopen the breaker, one step wider than last time.
    fn open(&mut self, now: Instant) {
        self.state = BreakerState::Open;
        self.opened_at = now;
        self.trips = self.trips.saturating_add(1);
    }
}

impl Inner {
    /// The cooldown a breaker that has tripped `trips` times in a row waits:
    /// the base doubled once per trip, capped. `trips == 0` is the cooldown a
    /// first trip would get, which is what [`LlmBreaker::cooldown`] reports to
    /// a caller asking how long a fresh breaker rests.
    fn cooldown_for(&self, trips: u32) -> Duration {
        let doublings = trips.saturating_sub(1).min(u32::BITS - 1);
        self.open_duration
            .saturating_mul(1u32 << doublings)
            .min(self.max_open_duration)
    }
}

impl LlmBreaker {
    /// Breaker with the default threshold and cooldown on the wall clock.
    pub fn new() -> Self {
        Self::with_clock(
            TRIP_THRESHOLD,
            OPEN_DURATION,
            MAX_OPEN_DURATION,
            Arc::new(SystemClock),
        )
    }

    fn with_clock(
        threshold: u32,
        open_duration: Duration,
        max_open_duration: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now();
        Self {
            inner: Arc::new(Inner {
                threshold,
                open_duration,
                max_open_duration,
                clock,
                machine: Mutex::new(Machine {
                    state: BreakerState::Closed,
                    consecutive_failures: 0,
                    opened_at: now,
                    trips: 0,
                }),
            }),
        }
    }

    /// The cooldown *currently* in force — the full `retry_after` right after a
    /// trip. Not a constant: it widens with each trip that a recovery probe
    /// failed to clear (see [`Machine::trips`]), so a caller that reports "retry
    /// in Ns" reports the number the breaker will actually hold to.
    pub fn cooldown(&self) -> Duration {
        let trips = self.lock().trips;
        self.inner.cooldown_for(trips.max(1))
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
                let cooldown = self.inner.cooldown_for(m.trips);
                let elapsed = now.saturating_duration_since(m.opened_at);
                if elapsed >= cooldown {
                    m.state = BreakerState::HalfOpen; // admit a single probe
                    Ok(())
                } else {
                    Err(BreakerOpen {
                        retry_after: cooldown - elapsed,
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
            // A half-open probe decides recovery. Closing is the only thing
            // that resets the escalation: the endpoint answered, so whatever
            // was wrong with it is over and the next outage starts from the
            // base cooldown again.
            (BreakerState::HalfOpen, Outcome::Success) => {
                m.state = BreakerState::Closed;
                m.consecutive_failures = 0;
                m.trips = 0;
            }
            (BreakerState::HalfOpen, Outcome::Failure) => m.open(now),
            // Any success clears the failure streak.
            (BreakerState::Closed, Outcome::Success) => {
                m.consecutive_failures = 0;
            }
            (BreakerState::Closed, Outcome::Failure) => {
                m.consecutive_failures += 1;
                if m.consecutive_failures >= self.inner.threshold {
                    m.open(now);
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

impl std::fmt::Debug for LlmBreaker {
    /// The state, and nothing else. The threshold and cooldown are constants,
    /// and the clock is an implementation detail; what a `{:?}` of a struct
    /// holding a breaker needs to answer is whether this endpoint is currently
    /// being dialed at all.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("LlmBreaker").field(&self.state()).finish()
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

    /// A breaker whose cooldown may escalate up to `cap`.
    fn capped(threshold: u32, open: Duration, cap: Duration) -> (LlmBreaker, Arc<MockClock>) {
        let clock = MockClock::new();
        (
            LlmBreaker::with_clock(threshold, open, cap, clock.clone()),
            clock,
        )
    }

    /// A breaker with room to escalate several times before the cap bites, for
    /// the tests that are about something else.
    fn breaker(threshold: u32, open: Duration) -> (LlmBreaker, Arc<MockClock>) {
        capped(threshold, open, open * 1024)
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

    /// Each failed recovery probe widens the next cooldown. A continuous run
    /// waits an open breaker out rather than ending on it, so a flat 30s would
    /// mean dialing a provider that is down for an hour 120 times; doubling
    /// turns that into single digits without slowing recovery from a blip,
    /// which still costs exactly one base cooldown.
    #[test]
    fn each_failed_probe_widens_the_next_cooldown() {
        let (cb, clock) = breaker(1, Duration::from_secs(30));
        cb.record(Outcome::Failure); // first trip
        assert_eq!(cb.cooldown(), Duration::from_secs(30));

        // Probe at +30s, and it fails: the breaker reopens for twice as long.
        clock.advance(Duration::from_secs(30));
        assert!(cb.check().is_ok());
        cb.record(Outcome::Failure);
        assert_eq!(cb.cooldown(), Duration::from_secs(60));
        clock.advance(Duration::from_secs(45));
        assert!(cb.check().is_err(), "45s is no longer enough");
        clock.advance(Duration::from_secs(15));
        assert!(cb.check().is_ok());

        // And again.
        cb.record(Outcome::Failure);
        assert_eq!(cb.cooldown(), Duration::from_secs(120));
    }

    /// The escalation is bounded, so a provider that is down all day is still
    /// re-probed on a schedule a human would call reasonable.
    #[test]
    fn the_escalated_cooldown_stops_at_the_cap() {
        let (cb, clock) = capped(
            1,
            Duration::from_secs(30),
            Duration::from_secs(120), // two doublings of room
        );
        cb.record(Outcome::Failure);
        for _ in 0..8 {
            clock.advance(Duration::from_secs(600));
            assert!(cb.check().is_ok(), "the cap always eventually elapses");
            cb.record(Outcome::Failure);
        }
        assert_eq!(cb.cooldown(), Duration::from_secs(120));
    }

    /// A probe that succeeds means the outage is over, so the *next* one starts
    /// from the base cooldown again rather than inheriting an hour of history.
    #[test]
    fn recovery_resets_the_escalation() {
        let (cb, clock) = breaker(1, Duration::from_secs(30));
        cb.record(Outcome::Failure);
        clock.advance(Duration::from_secs(30));
        assert!(cb.check().is_ok());
        cb.record(Outcome::Failure); // widened to 60s
        clock.advance(Duration::from_secs(60));
        assert!(cb.check().is_ok());
        cb.record(Outcome::Success); // recovered
        assert_eq!(cb.state(), BreakerState::Closed);

        cb.record(Outcome::Failure); // a fresh outage, later
        assert_eq!(
            cb.cooldown(),
            Duration::from_secs(30),
            "the next outage is judged on its own"
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
