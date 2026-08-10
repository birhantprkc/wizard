//! The one retry ladder every model call climbs.
//!
//! A provider that is down is down for everyone, so the policy for what to do
//! about it cannot live in the loop that happened to notice. This module holds
//! it once: consult the endpoint's circuit breaker before dialing, classify
//! the failure, feed the breaker, honour a server-stated `Retry-After` over
//! our own ladder, and stop climbing when the budget runs out or the breaker
//! opens.
//!
//! Before it existed the parent turn had all of that and
//! [`subagent::spawn`](super::subagent::spawn) had a hand-copied half of it —
//! the backoff, but no breaker — so a sub-run kept dialing an endpoint the
//! parent had already given up on. `/ultra` fans N candidates at that one
//! endpoint per turn, which is N loops discovering the same outage
//! independently and N × 7 requests spent proving it.
//!
//! What a caller still owns is what a call *is*; where a wait is *reported* is
//! the run's [`Sink`], the same one its tool calls and its text go to.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;

use crate::llm::{RetryAfter, TruncatedToolCall, retry_delay};

use super::turn::Sink;
use super::{AgentEvent, CancelHandle, breaker, cancelled, error_is_transient};

/// Total time one climb will spend sitting out breaker cooldowns and backoff
/// before it gives up, even when the run asked to wait outages out.
///
/// A ceiling is needed because [`error_is_transient`] *defaults* an error
/// nothing recognizes to transient — the right default, since the alternative
/// is ending a run over an error we simply have not met before — and some
/// genuinely permanent failures arrive unrecognized. An adapter that reports
/// "not signed in; run `wizard --login` first" as a bare `anyhow` error is
/// indistinguishable, from here, from a provider that is down, and it will go
/// on being indistinguishable forever. Without a bound a misconfigured
/// continuous run would sit in front of an endpoint it can never reach until
/// somebody happened to look, which is the complaint this change exists to fix
/// wearing a different hat.
///
/// Half a day: longer than any outage a hosted provider has actually had, and
/// short enough that a run which is never going to work is over, with a clean
/// circuit-breaker end and a rolled-back cycle, before the next morning.
const OUTAGE_PATIENCE: Duration = Duration::from_secs(12 * 60 * 60);

/// The policy one call climbs under.
pub(super) struct Ladder<'a> {
    /// Circuit breaker over the endpoint being dialed. Shared rather than
    /// per-loop: the parent and every subagent it fans out are talking to the
    /// same provider, so one of them learning it is down has to count for all
    /// of them.
    pub breaker: &'a breaker::LlmBreaker,
    /// Retries allowed after the first attempt. `None` climbs for as long as
    /// the breaker permits, which is what continuous mode wants and why the
    /// breaker is what bounds it.
    pub budget: Option<u32>,
    /// Whether an open breaker is an outage to *wait out* rather than a reason
    /// to stop, for up to [`OUTAGE_PATIENCE`].
    ///
    /// This is the difference between a session and a standing mission. A turn
    /// someone is watching wants to hear that the provider is down, right away,
    /// so they can go and do something else; ending the climb is the honest
    /// answer. A continuous run has nobody watching and nothing else to do, and
    /// its whole promise is to keep working through exactly this — so for it,
    /// the breaker's job is to stop the *hammering*, not to stop the *run*.
    ///
    /// It used to stop the run. Eight consecutive failures spaced by the
    /// default ladder (5s doubling to a 300s cap) is about ten minutes, so any
    /// provider blip longer than a coffee break ended a perpetual agent —
    /// which is one of the likeliest reasons one is found stopped in the
    /// morning. With this set the climb sleeps the breaker's (escalating)
    /// cooldown and takes the half-open probe when it comes up, which is the
    /// same traffic pattern the breaker wanted and none of the mortality.
    pub wait_out_outage: bool,
    pub base_secs: u64,
    pub max_secs: u64,
    /// The run's own wall-clock cap, when it has one, so that waiting out an
    /// outage cannot outlive it.
    ///
    /// The loop checks the deadline between steps, which is where it belongs —
    /// but a climb that may now sleep for hours is a step that can straddle it,
    /// and `--max-hours 8` has to mean eight hours even when hour seven is the
    /// one the provider went down in. Reaching it ends the climb the same way
    /// running out of patience does: cleanly, as an open breaker, so the step
    /// loop gets its turn to notice the deadline and say so properly.
    pub deadline: Option<std::time::Instant>,
    /// Raised mid-wait, ends the climb. `None` where something outside owns
    /// the interrupt — [`subagent::spawn`](super::subagent::spawn) races the
    /// whole sub-loop against the handle, so a second observation inside it
    /// would only decide the same thing later.
    pub cancel: Option<&'a CancelHandle>,
    /// Where this run reports. A turn's wait is the turn — the user is staring
    /// at a spinner that owes them an explanation — while a sub-run's is a log
    /// line, because its run-scoped events all belong to its own pane and there
    /// is no shape among them for "still waiting". Reporting one on the
    /// parent's channel would put N candidates' backoff notices into the
    /// transcript of a turn that has not produced a word yet.
    pub sink: &'a Sink,
}

impl Ladder<'_> {
    /// The breaker refused the call before it was made.
    async fn refused(&self, retry_after: Duration) {
        let secs = retry_after.as_secs();
        self.sink
            .error(format!(
                "LLM circuit breaker open; provider still unavailable (retry in {secs}s)"
            ))
            .await;
    }

    /// This attempt's failure is the one that opened the breaker.
    async fn tripped(&self) {
        self.sink
            .error("LLM circuit breaker tripped after repeated failures; ending".to_string())
            .await;
    }

    /// The breaker is open and this climb is going to sit it out. Reported
    /// every time round, not once: a run that goes quiet for a quarter of an
    /// hour is indistinguishable from a run that died, and the whole point of
    /// waiting instead of ending is that somebody can tell the difference.
    async fn waiting_out(&self, retry_after: Duration) {
        let secs = retry_after.as_secs();
        self.sink
            .error(format!(
                "LLM circuit breaker open; provider unavailable — waiting {secs}s then probing \
                 again (continuous run)"
            ))
            .await;
    }

    /// A run that *was* waiting the outage out has stopped: it has spent its
    /// patience, or the run's own deadline has come round. Distinct from
    /// [`Self::refused`], which is a climb that was never going to wait,
    /// because a line saying "retry in 900s" after half a day of waiting reads
    /// like the run is still trying.
    async fn gave_up(&self, waited: Duration) {
        let minutes = waited.as_secs() / 60;
        self.sink
            .error(format!(
                "LLM circuit breaker open; the provider has not answered in {minutes} minutes of \
                 waiting — giving up on this cycle"
            ))
            .await;
    }

    /// About to sleep `delay` before attempt number `attempt`.
    async fn waiting(&self, err: &anyhow::Error, delay: Duration, attempt: u32, stated: bool) {
        let secs = delay.as_secs_f64();
        let source = if stated {
            " (server-stated Retry-After)"
        } else {
            ""
        };
        // The retry re-generates the response from the top; tell consumers to
        // drop any partial text this attempt streamed.
        self.sink.turn_event(AgentEvent::StreamRetrying).await;
        self.sink
            .error(format!(
                "LLM unavailable ({err:#}); sleeping {secs:.1}s{source} then retrying \
                 (attempt {attempt})"
            ))
            .await;
    }
}

/// What a climb produced.
#[derive(Debug)]
pub(super) enum Climbed<T> {
    Done(T),
    /// The user interrupted, either during the call or during a wait. Not an
    /// error and not a provider outcome: it must never reach the breaker, or
    /// a user who interrupts often enough takes the endpoint down for
    /// themselves.
    Cancelled,
}

impl Ladder<'_> {
    /// Run `call` until it succeeds, is refused, or runs out of ladder.
    ///
    /// `call` reports its own cancellation rather than having it inferred,
    /// because only the caller knows what an interrupted call looks like: the
    /// parent's streams one and stops mid-chunk, while a sub-loop's cannot be
    /// interrupted from in here at all.
    ///
    /// Errors come back exactly as `call` raised them — the ladder adds no
    /// context of its own, so a caller can still name the run in the message
    /// its own way — except when the breaker ends the climb, which is
    /// [`breaker::LlmBreakerOpen`] and is a *clean* stop rather than a
    /// failure.
    ///
    /// `Fn() -> Fut` rather than an `AsyncFn` bound, deliberately: the futures
    /// this drives are spawned onto the runtime by a background subagent, and
    /// `Send` leaks out through a named future type where it cannot through
    /// the higher-ranked one an `AsyncFn` bound hides.
    pub(super) async fn climb<T, F, Fut>(&self, call: F) -> Result<Climbed<T>>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Climbed<T>>>,
    {
        let mut attempt: u32 = 0;
        // Everything this climb has slept, cooldowns and backoff alike. What it
        // bounds is [`OUTAGE_PATIENCE`]; a climb that is not waiting outages out
        // is bounded by its budget long before this matters.
        let mut waited = Duration::ZERO;
        loop {
            // Whether an open breaker is still something to sit out, or whether
            // this climb has now spent as long in front of a silent endpoint as
            // it is ever going to — either its own patience, or the whole run's.
            let patient = self.wait_out_outage
                && waited < OUTAGE_PATIENCE
                && self
                    .deadline
                    .is_none_or(|deadline| tokio::time::Instant::now().into_std() < deadline);
            // Fail fast when the breaker is open (tripped by this loop, an
            // earlier turn, or a sibling subagent): don't dial a provider that
            // is down. Past the cooldown, `check` admits a single recovery
            // probe.
            if let Err(open) = self.breaker.check() {
                if !patient {
                    if self.wait_out_outage {
                        self.gave_up(waited).await;
                    } else {
                        self.refused(open.retry_after).await;
                    }
                    return Err(breaker::LlmBreakerOpen {
                        retry_after: open.retry_after,
                    }
                    .into());
                }
                // Sit out the remaining cooldown and go round again; the next
                // `check` is the one that admits the probe. Sleeping exactly
                // the stated remainder is enough because the breaker admits at
                // `elapsed >= cooldown`, and the cooldown only ever widens
                // under us, which this loop discovers by asking again.
                self.waiting_out(open.retry_after).await;
                waited += open.retry_after;
                tokio::select! {
                    biased;
                    () = cancelled(self.cancel) => return Ok(Climbed::Cancelled),
                    () = tokio::time::sleep(open.retry_after) => {}
                }
                continue;
            }
            match call().await {
                Ok(Climbed::Cancelled) => return Ok(Climbed::Cancelled),
                Ok(done) => {
                    self.breaker.record(breaker::Outcome::Success);
                    return Ok(done);
                }
                Err(err) => {
                    // A reply truncated mid tool call is not an outage: the
                    // provider answered, the answer was just unusable. Nothing
                    // about a retry changes that, and every retry re-bills the
                    // whole prompt, so it ends the climb like any other
                    // permanent error (auth, bad request, missing model), and
                    // never feeds the breaker.
                    if err.is::<TruncatedToolCall>() || !error_is_transient(&err) {
                        return Err(err);
                    }
                    self.breaker.record(breaker::Outcome::Failure);
                    if self.budget.is_some_and(|budget| attempt >= budget) {
                        return Err(err);
                    }
                    // If that failure just tripped the breaker, stop climbing
                    // the ladder now rather than sleeping a full backoff before
                    // the next `check` would catch it. For a budgeted climb
                    // that is the end of it — this is what bounds an unbudgeted
                    // one too when nobody asked us to wait. A run that did ask
                    // goes back to the top, where the cooldown is waited out.
                    if self.breaker.is_open() {
                        if !patient {
                            self.tripped().await;
                            return Err(breaker::LlmBreakerOpen {
                                retry_after: self.breaker.cooldown(),
                            }
                            .into());
                        }
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    // A rate limiter that told us when to come back knows
                    // better than our ladder does; anything else climbs the
                    // ladder. Either way the wait is jittered (see
                    // `retry_delay`), which is what stops N candidates that
                    // shared one 429 from waking in lockstep to re-storm the
                    // endpoint together.
                    let retry_after = err.downcast_ref::<RetryAfter>().map(|hint| hint.0);
                    let delay = retry_delay(attempt, self.base_secs, self.max_secs, retry_after);
                    self.waiting(&err, delay, attempt + 1, retry_after.is_some())
                        .await;
                    waited += delay;
                    tokio::select! {
                        biased;
                        () = cancelled(self.cancel) => return Ok(Climbed::Cancelled),
                        () = tokio::time::sleep(delay) => {}
                    }
                    // Saturating because a run that waits out a long outage
                    // goes round this loop as many times as the outage lasts,
                    // and the only thing the count feeds is a delay that has
                    // been pinned at `max_secs` since attempt six.
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A run sink, whose waits are log lines.
    fn sink() -> Sink {
        Sink::Run {
            run: 1,
            name: "test".to_string(),
            events: None,
        }
    }

    /// A ladder that reports into a log, over a fresh breaker.
    fn ladder<'a>(
        breaker: &'a breaker::LlmBreaker,
        budget: Option<u32>,
        sink: &'a Sink,
    ) -> Ladder<'a> {
        Ladder {
            breaker,
            budget,
            wait_out_outage: false,
            deadline: None,
            base_secs: 0,
            max_secs: 0,
            cancel: None,
            sink,
        }
    }

    /// A continuous run's ladder: no budget, and an open breaker is something
    /// to sit out rather than to die on.
    fn perpetual<'a>(breaker: &'a breaker::LlmBreaker, sink: &'a Sink) -> Ladder<'a> {
        Ladder {
            wait_out_outage: true,
            ..ladder(breaker, None, sink)
        }
    }

    fn transient() -> anyhow::Error {
        crate::llm::http_error_with_retry_after(503, "scripted failure", None)
    }

    /// The breaker is the thing that bounds an unbudgeted climb, and it counts
    /// across climbs: a second call against an endpoint the first one proved
    /// dead must not start the count again.
    #[tokio::test(start_paused = true)]
    async fn failures_accumulate_across_climbs_until_the_breaker_ends_them() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let calls = Mutex::new(0u32);

        // Seven failures: the shipped trip threshold is eight, so a single
        // interactive budget's worth of failures still surfaces the provider's
        // own error rather than a breaker stop.
        let err = ladder(&breaker, Some(6), &sink)
            .climb(|| async {
                *calls.lock().unwrap() += 1;
                Err::<Climbed<()>, _>(transient())
            })
            .await
            .expect_err("the budget bounds the climb");
        assert!(!err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(*calls.lock().unwrap(), 7, "initial attempt plus the budget");
        assert!(!breaker.is_open());

        // The eighth trips it, and the climb ends on the trip rather than on
        // its own budget.
        let err = ladder(&breaker, Some(6), &sink)
            .climb(|| async {
                *calls.lock().unwrap() += 1;
                Err::<Climbed<()>, _>(transient())
            })
            .await
            .expect_err("the breaker ends the climb");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(*calls.lock().unwrap(), 8, "one more call, then the trip");

        // And once open it refuses without dialing at all.
        let err = ladder(&breaker, Some(6), &sink)
            .climb(|| async {
                *calls.lock().unwrap() += 1;
                Err::<Climbed<()>, _>(transient())
            })
            .await
            .expect_err("an open breaker refuses");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert_eq!(*calls.lock().unwrap(), 8, "no call was made");
    }

    /// The complaint this exists for: a continuous run stopping in the night.
    ///
    /// Eight consecutive failures is about ten minutes of backoff on the
    /// default ladder, and a provider blip that lasted longer than that used to
    /// trip the breaker, return `LlmBreakerOpen`, and end the whole standing
    /// mission. A run that
    /// waits outages out instead keeps climbing across the trip, sits out the
    /// cooldown, takes the half-open probe, and finishes the call the moment
    /// the provider comes back.
    #[tokio::test(start_paused = true)]
    async fn a_continuous_run_waits_an_outage_out_instead_of_ending_on_it() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        // Down for long enough to trip the breaker (8) and to fail the first
        // recovery probe as well, so the escalating cooldown is exercised too.
        let calls = Mutex::new(0u32);
        let outcome = perpetual(&breaker, &sink)
            .climb(|| async {
                let mut calls = calls.lock().unwrap();
                *calls += 1;
                if *calls <= 9 {
                    Err(transient())
                } else {
                    Ok(Climbed::Done(*calls))
                }
            })
            .await
            .expect("the outage is waited out, not surfaced");
        assert!(matches!(outcome, Climbed::Done(10)));
        assert_eq!(
            breaker.state(),
            breaker::BreakerState::Closed,
            "the probe that succeeded closed it"
        );
    }

    /// The same outage on an interactive turn still ends the climb: somebody is
    /// watching a spinner and deserves to be told the provider is down rather
    /// than have it silently wait a quarter of an hour.
    #[tokio::test(start_paused = true)]
    async fn a_watched_turn_still_ends_on_the_trip() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let err = ladder(&breaker, None, &sink)
            .climb(|| async { Err::<Climbed<()>, _>(transient()) })
            .await
            .expect_err("an unbudgeted but unwatched-for-outages climb ends on the trip");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
    }

    /// Waiting out an outage is still interruptible: the cooldown sleep races
    /// the run's cancel handle exactly like the backoff sleep does, or a
    /// continuous run would ignore Ctrl-C for up to a quarter of an hour.
    #[tokio::test(start_paused = true)]
    async fn waiting_out_an_outage_still_observes_the_interrupt() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let cancel = CancelHandle::default();
        // Open it first, so the very next climb starts in the waiting branch.
        for _ in 0..16 {
            breaker.record(breaker::Outcome::Failure);
        }
        assert!(breaker.is_open());

        let ladder = Ladder {
            cancel: Some(&cancel),
            wait_out_outage: true,
            ..ladder(&breaker, None, &sink)
        };
        cancel.cancel();
        let outcome = ladder
            .climb(|| async { Ok(Climbed::<()>::Done(())) })
            .await
            .expect("cancellation is not an error");
        assert!(matches!(outcome, Climbed::Cancelled));
    }

    /// Patience is not infinite, and it must not be.
    ///
    /// `error_is_transient` defaults an unrecognized error to transient, which
    /// is the right default and means some permanent failures — an adapter
    /// reporting "not signed in" as a bare error, a proxy 5xx-ing a request it
    /// will never accept — are indistinguishable from an outage and will stay
    /// that way. A run that waited on one forever would be stopped in every
    /// sense that matters while looking like it was still going.
    #[tokio::test(start_paused = true)]
    async fn even_a_continuous_run_gives_up_eventually() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let calls = Mutex::new(0u32);
        let started = tokio::time::Instant::now();
        let err = perpetual(&breaker, &sink)
            .climb(|| async {
                *calls.lock().unwrap() += 1;
                Err::<Climbed<()>, _>(transient())
            })
            .await
            .expect_err("an endpoint that never answers ends the climb in the end");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert!(
            started.elapsed() >= OUTAGE_PATIENCE,
            "and only after it has really waited: {:?}",
            started.elapsed()
        );
        // Half a day of escalating cooldowns is a handful of dials an hour, not
        // a request every thirty seconds for twelve hours.
        let calls = *calls.lock().unwrap();
        assert!(calls < 100, "{calls} calls in half a day is too many");
    }

    /// `--max-hours 8` has to mean eight hours even when hour seven is the one
    /// the provider went down in. The step loop checks the deadline between
    /// steps, which was fine when no step could sleep for long, and is not now
    /// that one can sit out an outage — so the climb observes it too.
    #[tokio::test(start_paused = true)]
    async fn waiting_out_an_outage_never_outlives_the_runs_deadline() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        for _ in 0..16 {
            breaker.record(breaker::Outcome::Failure);
        }
        assert!(breaker.is_open());

        let started = tokio::time::Instant::now();
        let err = Ladder {
            deadline: Some(started.into_std() + Duration::from_secs(60)),
            ..perpetual(&breaker, &sink)
        }
        .climb(|| async { Err::<Climbed<()>, _>(transient()) })
        .await
        .expect_err("past the deadline the climb ends as an open breaker");
        assert!(err.is::<breaker::LlmBreakerOpen>(), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(600),
            "it stopped near the deadline, not at the patience ceiling: {:?}",
            started.elapsed()
        );
    }

    /// A cancelled call is the user's decision, not the provider's, so it
    /// leaves the breaker exactly where it found it.
    #[tokio::test]
    async fn a_cancelled_call_is_not_a_provider_outcome() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let outcome = ladder(&breaker, Some(6), &sink)
            .climb(|| async { Ok(Climbed::<()>::Cancelled) })
            .await
            .expect("cancellation is not an error");
        assert!(matches!(outcome, Climbed::Cancelled));
        assert_eq!(breaker.state(), breaker::BreakerState::Closed);
    }

    /// A permanent error ends the climb on the first attempt and never feeds
    /// the breaker: an endpoint that rejects our credentials is not an
    /// endpoint that is down.
    #[tokio::test]
    async fn a_permanent_error_ends_the_climb_without_touching_the_breaker() {
        let breaker = breaker::LlmBreaker::new();
        let sink = sink();
        let calls = Mutex::new(0u32);
        let err = ladder(&breaker, Some(6), &sink)
            .climb(|| async {
                *calls.lock().unwrap() += 1;
                Err::<Climbed<()>, _>(crate::llm::http_error_with_retry_after(401, "nope", None))
            })
            .await
            .expect_err("a 401 fails the call");
        assert!(format!("{err:#}").contains("nope"));
        assert_eq!(*calls.lock().unwrap(), 1, "a 401 is never retried");
        assert_eq!(breaker.state(), breaker::BreakerState::Closed);
    }
}
