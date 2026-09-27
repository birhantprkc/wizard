//! The ChatGPT plan's rate limits: the 5-hour and weekly windows the Codex CLI
//! shows in `/status`.
//!
//! Three places report them, and each is read the way codex-rs reads it:
//!
//! - every `/responses` reply carries `x-codex-primary-used-percent`,
//!   `x-codex-primary-window-minutes` and `x-codex-primary-reset-at` (unix
//!   seconds), and the same three for `secondary`
//!   (`codex-api/src/rate_limits.rs`, `parse_rate_limit_for_limit`);
//! - a `codex.rate_limits` event with `plan_type` and
//!   `rate_limits.{primary,secondary}.{used_percent,window_minutes,reset_at}`.
//!   Codex reads it on its websocket transport; this client streams SSE, so it
//!   is parsed there in case the stream carries it too;
//! - `GET /backend-api/wham/usage`, which is what Codex's `/status` asks and
//!   costs no turn (`backend-client/src/client/rate_limit_resets.rs`). Its
//!   windows are `rate_limit.{primary,secondary}_window` with `used_percent`,
//!   `limit_window_seconds` and `reset_at`, next to a live `plan_type`.
//!
//! Only the default `codex` limit family is read. The server can also send
//! per-model families (`x-codex-<name>-primary-used-percent`); those are not
//! the plan's 5-hour and weekly budget and are left out.

use std::sync::Mutex;

use reqwest::header::HeaderMap;
use serde_json::Value;

/// One rate-limit window as the endpoint reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// 0-100.
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

/// The plan's windows, plus the plan when the source named it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Limits {
    /// The short window, 5 hours on the paid plans.
    pub primary: Option<Window>,
    /// The long one, a week on the paid plans.
    pub secondary: Option<Window>,
    /// `plus`, `pro`, `team`, … as the endpoint spells it.
    pub plan_type: Option<String>,
}

impl Limits {
    fn has_windows(&self) -> bool {
        self.primary.is_some() || self.secondary.is_some()
    }
}

/// The most recent limits any ChatGPT reply in this process carried.
#[cfg(not(test))]
static LATEST: Mutex<Option<Limits>> = Mutex::new(None);

#[cfg(not(test))]
fn with_latest<R>(f: impl FnOnce(&mut Option<Limits>) -> R) -> R {
    f(&mut LATEST.lock().unwrap_or_else(|e| e.into_inner()))
}

// Per test thread, so two tests recording at once cannot read each other's.
#[cfg(test)]
thread_local! {
    static LATEST: Mutex<Option<Limits>> = const { Mutex::new(None) };
}

#[cfg(test)]
fn with_latest<R>(f: impl FnOnce(&mut Option<Limits>) -> R) -> R {
    LATEST.with(|latest| f(&mut latest.lock().unwrap_or_else(|e| e.into_inner())))
}

/// Remember `limits` as the newest reading. A reading without a plan keeps the
/// plan an earlier one named: the headers never carry it, the event and the
/// usage endpoint do.
pub fn record(mut limits: Limits) {
    with_latest(|latest| {
        if limits.plan_type.is_none() {
            limits.plan_type = latest.as_ref().and_then(|old| old.plan_type.clone());
        }
        *latest = Some(limits);
    });
}

/// The newest reading this process has seen, if any.
pub fn latest() -> Option<Limits> {
    with_latest(|latest| latest.clone())
}

/// Read the default `x-codex-*` header family. `None` when the reply carried
/// no window with data.
pub fn from_headers(headers: &HeaderMap) -> Option<Limits> {
    let limits = Limits {
        primary: header_window(headers, "primary"),
        secondary: header_window(headers, "secondary"),
        plan_type: None,
    };
    limits.has_windows().then_some(limits)
}

fn header_window(headers: &HeaderMap, which: &str) -> Option<Window> {
    let text = |suffix: &str| {
        headers
            .get(format!("x-codex-{which}-{suffix}"))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let used_percent = text("used-percent")?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())?;
    let window_minutes = text("window-minutes").and_then(|value| value.parse::<i64>().ok());
    let resets_at = text("reset-at").and_then(|value| value.parse::<i64>().ok());
    // Codex's rule: an all-zero window with no reset is the header family
    // being present with nothing to say, not a window at 0%.
    let has_data =
        used_percent != 0.0 || window_minutes.is_some_and(|m| m != 0) || resets_at.is_some();
    has_data.then_some(Window {
        used_percent,
        window_minutes,
        resets_at,
    })
}

/// Read a `codex.rate_limits` stream event. `None` for any other event, and
/// for one that names no window.
pub fn from_event(event: &Value) -> Option<Limits> {
    if event.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
        return None;
    }
    let details = event.get("rate_limits");
    let window = |key: &str| {
        let raw = details?.get(key)?;
        Some(Window {
            used_percent: finite(raw.get("used_percent"))?,
            window_minutes: raw.get("window_minutes").and_then(Value::as_i64),
            resets_at: raw.get("reset_at").and_then(Value::as_i64),
        })
    };
    let limits = Limits {
        primary: window("primary"),
        secondary: window("secondary"),
        plan_type: plan(event.get("plan_type")),
    };
    limits.has_windows().then_some(limits)
}

/// Read a `/wham/usage` body. `now` (unix seconds) turns a bare
/// `reset_after_seconds` into a moment when `reset_at` is missing.
pub fn from_usage_body(body: &Value, now: i64) -> Option<Limits> {
    let rate_limit = body.get("rate_limit");
    let window = |key: &str| {
        let raw = rate_limit?.get(key)?;
        let resets_at = raw.get("reset_at").and_then(Value::as_i64).or_else(|| {
            raw.get("reset_after_seconds")
                .and_then(Value::as_i64)
                .map(|after| now + after)
        });
        Some(Window {
            used_percent: finite(raw.get("used_percent"))?,
            window_minutes: raw
                .get("limit_window_seconds")
                .and_then(Value::as_i64)
                .filter(|seconds| *seconds > 0)
                .map(|seconds| seconds / 60),
            resets_at,
        })
    };
    let limits = Limits {
        primary: window("primary_window"),
        secondary: window("secondary_window"),
        plan_type: plan(body.get("plan_type")),
    };
    // A body with a plan and no windows is still an answer: a free account
    // with nothing metered yet. The caller shows the plan and says so.
    (limits.has_windows() || limits.plan_type.is_some()).then_some(limits)
}

fn finite(value: Option<&Value>) -> Option<f64> {
    value?
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

fn plan(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|plan| !plan.is_empty() && *plan != "unknown")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    /// Headers as a Plus account's `/responses` reply carries them.
    fn recorded_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1790550600"),
            ("x-codex-secondary-used-percent", "41"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1790985600"),
            ("x-codex-credits-has-credits", "false"),
            ("content-type", "text/event-stream"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn response_headers_become_both_windows() {
        let limits = from_headers(&recorded_headers()).expect("limits");
        assert_eq!(
            limits.primary,
            Some(Window {
                used_percent: 12.5,
                window_minutes: Some(300),
                resets_at: Some(1_790_550_600),
            })
        );
        assert_eq!(
            limits.secondary,
            Some(Window {
                used_percent: 41.0,
                window_minutes: Some(10080),
                resets_at: Some(1_790_985_600),
            })
        );
        assert_eq!(limits.plan_type, None);
    }

    #[test]
    fn a_reply_without_the_headers_is_no_reading() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        assert_eq!(from_headers(&headers), None);
    }

    #[test]
    fn an_all_zero_window_is_absent_not_zero_percent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("0"),
        );
        headers.insert(
            "x-codex-primary-window-minutes",
            HeaderValue::from_static("0"),
        );
        assert_eq!(from_headers(&headers), None);
    }

    #[test]
    fn a_garbled_percent_is_skipped() {
        let mut headers = recorded_headers();
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("NaN"),
        );
        let limits = from_headers(&headers).expect("secondary still reads");
        assert_eq!(limits.primary, None);
        assert!(limits.secondary.is_some());
    }

    #[test]
    fn the_rate_limit_event_carries_the_plan() {
        let event = serde_json::json!({
            "type": "codex.rate_limits",
            "plan_type": "pro",
            "rate_limits": {
                "primary": {"used_percent": 3.0, "window_minutes": 300, "reset_at": 1790550600},
                "secondary": {"used_percent": 20.0, "window_minutes": 10080, "reset_at": 1790985600}
            },
            "credits": {"has_credits": false, "unlimited": false, "balance": null}
        });
        let limits = from_event(&event).expect("limits");
        assert_eq!(limits.plan_type.as_deref(), Some("pro"));
        assert_eq!(limits.primary.expect("primary").used_percent, 3.0);
        assert_eq!(
            limits.secondary.expect("secondary").window_minutes,
            Some(10080)
        );
        assert_eq!(
            from_event(&serde_json::json!({"type": "response.completed"})),
            None
        );
    }

    #[test]
    fn the_usage_endpoint_body_maps_seconds_to_minutes() {
        let body = serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 7,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 9000,
                    "reset_at": 1790550600
                },
                "secondary_window": {
                    "used_percent": 33,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 400000
                }
            },
            "credits": {"has_credits": false, "unlimited": false},
            "account_id": "acct-should-not-leak",
            "user_id": "user-should-not-leak"
        });
        let limits = from_usage_body(&body, 1_790_000_000).expect("limits");
        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
        let primary = limits.primary.expect("primary");
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, Some(1_790_550_600));
        let secondary = limits.secondary.expect("secondary");
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(
            secondary.resets_at,
            Some(1_790_400_000),
            "reset_after fallback"
        );
    }

    #[test]
    fn a_usage_body_with_nothing_in_it_is_no_reading() {
        assert_eq!(from_usage_body(&serde_json::json!({}), 0), None);
        assert_eq!(
            from_usage_body(&serde_json::json!({"plan_type": "unknown"}), 0),
            None
        );
    }

    #[test]
    fn a_header_reading_keeps_the_plan_an_earlier_event_named() {
        record(Limits {
            primary: None,
            secondary: None,
            plan_type: Some("team".into()),
        });
        record(from_headers(&recorded_headers()).expect("limits"));
        let latest = latest().expect("latest");
        assert_eq!(latest.plan_type.as_deref(), Some("team"));
        assert_eq!(latest.primary.expect("primary").used_percent, 12.5);
    }
}
