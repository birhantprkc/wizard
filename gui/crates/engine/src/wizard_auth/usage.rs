//! Wizard's subscription usage for Settings → Wizard: the plan limits of each
//! account Wizard is signed in to (xAI, ChatGPT).
//!
//! Read by running `wizard usage --subscriptions --json` on the device rather
//! than by reading the token files here. Wizard owns those files: it locks the
//! xAI one across processes while it refreshes, and it knows not to refresh the
//! ChatGPT one from a second process. A copy of that logic in the engine would
//! be one more process able to spend a single-use refresh token under a running
//! session. The subprocess is also what `wizard --login` already is to this
//! page, and it needs no live ACP session.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_proto::AgentUsageWindow;

/// Wizard gives each account 10 s and asks them at once.
const USAGE_TIMEOUT: Duration = Duration::from_secs(25);

/// Every subscription the device's Wizard is signed in to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardUsage {
    pub subscriptions: Vec<SubscriptionUsage>,
}

/// One subscription, ready to draw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionUsage {
    /// `xai` or `chatgpt`.
    pub id: String,
    /// `xAI` or `ChatGPT`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Short window first.
    #[serde(default)]
    pub windows: Vec<AgentUsageWindow>,
    /// "build 12% · chat 7% · voice 1%", when the account splits its usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub products: Option<String>,
    /// Why there are no windows, or where they came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `wizard usage --subscriptions --json`, as Wizard prints it (version 1).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    version: u32,
    #[serde(default)]
    subscriptions: Vec<Subscription>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Subscription {
    id: String,
    name: String,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    windows: Vec<Window>,
    #[serde(default)]
    products: Vec<Product>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    label: String,
    used_percent: f64,
    #[serde(default)]
    resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Product {
    label: String,
    used_percent: f64,
}

/// Parse Wizard's JSON. A newer major version is refused rather than
/// half-read.
pub fn parse(stdout: &str) -> Result<WizardUsage> {
    let report: Report =
        serde_json::from_str(stdout.trim()).context("Wizard's usage report was not JSON")?;
    if report.version != 1 {
        bail!(
            "Wizard reports usage in format {}; update Wizard GUI to read it",
            report.version
        );
    }
    let subscriptions = report
        .subscriptions
        .into_iter()
        .map(|subscription| {
            let windows = subscription
                .windows
                .into_iter()
                .filter(|window| window.used_percent.is_finite())
                .map(|window| AgentUsageWindow {
                    label: meter_label(&window.label),
                    used_fraction: (window.used_percent / 100.0).clamp(0.0, 1.0) as f32,
                    resets_at: window.resets_at,
                })
                .collect::<Vec<_>>();
            let products = (!subscription.products.is_empty()).then(|| {
                subscription
                    .products
                    .iter()
                    .map(|product| format!("{} {}", product.label, percent(product.used_percent)))
                    .collect::<Vec<_>>()
                    .join(" · ")
            });
            let note = subscription.note.or_else(|| {
                (subscription.source.as_deref() == Some("lastReply") && !windows.is_empty())
                    .then(|| "As of the last reply".to_string())
            });
            SubscriptionUsage {
                id: subscription.id,
                name: subscription.name,
                plan: subscription.plan,
                windows,
                products,
                note,
            }
        })
        .collect();
    Ok(WizardUsage { subscriptions })
}

/// The meter column is narrow: `weekly` → `Week`, `5h` stays.
fn meter_label(label: &str) -> String {
    match label {
        "daily" => "Day".into(),
        "weekly" => "Week".into(),
        "monthly" => "Month".into(),
        "annual" => "Year".into(),
        "this period" => "Period".into(),
        other => other.to_string(),
    }
}

fn percent(number: f64) -> String {
    if number.fract().abs() < 0.05 {
        format!("{}%", number.round() as i64)
    } else {
        format!("{number:.1}%")
    }
}

/// Run `wizard usage --subscriptions --json` on this device.
pub async fn load() -> Result<WizardUsage> {
    let wizard = zeron_harness::AcpHarness::wizard()
        .cli_path()
        .ok_or_else(|| anyhow!("Wizard isn't installed on this device"))?;
    let run = tokio::process::Command::new(&wizard)
        .args(["usage", "--subscriptions", "--json"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(USAGE_TIMEOUT, run)
        .await
        .map_err(|_| anyhow!("Wizard took too long to report usage"))?
        .with_context(|| format!("couldn't start {}", wizard.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("--subscriptions") {
            bail!("This Wizard is too old to report subscription usage. Update it, then refresh.");
        }
        let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
        bail!(
            "Wizard couldn't report usage{}",
            last.map(|line| format!(": {}", line.trim()))
                .unwrap_or_default()
        );
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `wizard usage --subscriptions --json` printed for an xAI sign-in,
    /// plus a ChatGPT block from Wizard's own fixtures.
    const RECORDED: &str = r#"{"version":1,"subscriptions":[{"id":"xai","name":"xAI","plan":"SuperGrok Heavy","windows":[{"label":"weekly","usedPercent":20.0,"windowMinutes":10080,"resetsAt":"2026-10-01T21:06:39.232166Z"}],"products":[{"label":"build","usedPercent":12.0},{"label":"chat","usedPercent":6.0},{"label":"voice","usedPercent":2.0}],"source":"account"},{"id":"chatgpt","name":"ChatGPT","plan":"Plus","windows":[{"label":"5h","usedPercent":12.5,"windowMinutes":300,"resetsAt":"2026-09-27T23:10:00Z"},{"label":"weekly","usedPercent":41.0,"windowMinutes":10080,"resetsAt":"2026-10-03T00:00:00Z"}],"source":"lastReply"}]}"#;

    #[test]
    fn both_subscriptions_become_meters() {
        let usage = parse(RECORDED).expect("parses");
        assert_eq!(usage.subscriptions.len(), 2);
        let xai = &usage.subscriptions[0];
        assert_eq!(xai.name, "xAI");
        assert_eq!(xai.plan.as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(xai.windows.len(), 1);
        assert_eq!(xai.windows[0].label, "Week");
        assert!((xai.windows[0].used_fraction - 0.20).abs() < 1e-6);
        assert_eq!(
            xai.windows[0].resets_at.map(|at| at.timestamp()),
            Some(1_790_888_799)
        );
        assert_eq!(
            xai.products.as_deref(),
            Some("build 12% · chat 6% · voice 2%")
        );
        assert_eq!(xai.note, None);

        let chatgpt = &usage.subscriptions[1];
        let labels: Vec<&str> = chatgpt.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5h", "Week"]);
        assert!((chatgpt.windows[0].used_fraction - 0.125).abs() < 1e-6);
        assert_eq!(chatgpt.products, None);
        assert_eq!(chatgpt.note.as_deref(), Some("As of the last reply"));
    }

    #[test]
    fn a_signed_in_account_without_a_reading_keeps_its_note() {
        let usage = parse(
            r#"{"version":1,"subscriptions":[{"id":"chatgpt","name":"ChatGPT","plan":"Pro","windows":[],"note":"no request yet this session; limits show after the first reply"}]}"#,
        )
        .expect("parses");
        let chatgpt = &usage.subscriptions[0];
        assert!(chatgpt.windows.is_empty());
        assert_eq!(
            chatgpt.note.as_deref(),
            Some("no request yet this session; limits show after the first reply")
        );
    }

    #[test]
    fn nothing_signed_in_is_an_empty_list() {
        assert_eq!(
            parse("{\"version\":1,\"subscriptions\":[]}\n").expect("parses"),
            WizardUsage::default()
        );
    }

    #[test]
    fn over_a_hundred_percent_fills_the_bar_and_no_more() {
        let usage = parse(
            r#"{"version":1,"subscriptions":[{"id":"xai","name":"xAI","windows":[{"label":"weekly","usedPercent":130}]}]}"#,
        )
        .expect("parses");
        assert_eq!(usage.subscriptions[0].windows[0].used_fraction, 1.0);
    }

    #[test]
    fn a_future_format_or_garbage_is_an_error_not_an_empty_page() {
        let err = parse(r#"{"version":2,"subscriptions":[]}"#).expect_err("v2");
        assert!(err.to_string().contains("update Wizard GUI"), "{err}");
        assert!(parse("error: unexpected argument").is_err());
    }
}
