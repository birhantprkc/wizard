//! The progress bar under an agent that is installing, in onboarding and
//! Settings → Agents: what the installer is doing, how far along it is, and
//! on failure the installer's last lines with a Retry.

use std::time::Duration;

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, div,
    prelude::FluentBuilder as _, px,
};
use zeron_harness::install_progress::InstallProgress;
use zeron_rpc::methods;

use crate::state::EngineHandle;
use crate::theme::Theme;

const POLL: Duration = Duration::from_millis(250);

/// Ask the engine where `params`' install is every 250ms and hand each answer
/// to `apply`, until `apply` returns false. An engine too old to answer reads
/// as no progress, which renders as the start of the bar.
pub(crate) fn poll<T: 'static>(
    engine: EngineHandle,
    params: serde_json::Value,
    cx: &mut Context<T>,
    apply: impl Fn(&mut T, Option<InstallProgress>) -> bool + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            let progress = engine
                .client()
                .call(methods::INSTALL_PROGRESS, params.clone())
                .await
                .ok()
                .and_then(|value| serde_json::from_value::<Option<InstallProgress>>(value).ok())
                .flatten();
            let keep = this
                .update(cx, |view, cx| {
                    let keep = apply(view, progress);
                    cx.notify();
                    keep
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
            cx.background_executor().timer(POLL).await;
        }
    })
}

/// Status line, bar and detail for an install in flight.
pub(crate) fn render(
    theme: &Theme,
    id: impl Into<SharedString>,
    progress: Option<&InstallProgress>,
) -> AnyElement {
    let id = id.into();
    let (status, fraction, detail) = match progress {
        Some(p) => (
            p.status.clone(),
            p.fraction.clamp(0.0, 1.0),
            p.detail.clone(),
        ),
        None => ("Checking…".to_string(), 0.0, None),
    };
    div()
        .id(id.clone())
        .w_full()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .text_size(crate::typography::ui_rems(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(crate::life::life_spinner(
                    format!("{id}-life"),
                    6.0,
                    theme.text_muted,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text)
                        .child(SharedString::from(status)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(percent(fraction))),
                ),
        )
        .child(bar(theme, fraction))
        .when_some(detail, |el, detail| {
            el.child(
                div()
                    .truncate()
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(SharedString::from(detail)),
            )
        })
        .into_any_element()
}

fn bar(theme: &Theme, fraction: f32) -> AnyElement {
    div()
        .w_full()
        .h(px(6.0))
        .rounded_full()
        .overflow_hidden()
        .bg(crate::theme::ink(0.08))
        .when(fraction > 0.0, |el| {
            el.child(
                div()
                    .h_full()
                    // A sliver shows the bar is live before the first byte.
                    .w(gpui::relative(fraction.max(0.02)))
                    .rounded_full()
                    .bg(theme.accent),
            )
        })
        .into_any_element()
}

fn percent(fraction: f32) -> String {
    format!("{}%", (fraction * 100.0).floor() as u32)
}

/// A failed install: the reason, the installer's last lines, and Retry.
pub(crate) fn render_error(
    theme: &Theme,
    id: impl Into<SharedString>,
    error: &str,
    retry: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let (reason, output) = split_error(error);
    div()
        .id(id.into())
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .text_size(crate::typography::ui_rems(12.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.danger)
                        .child(SharedString::from(reason)),
                )
                .child(
                    crate::popover::btn_primary(theme, "Retry")
                        .id("install-retry")
                        .flex_none()
                        .on_click(retry),
                ),
        )
        .when(!output.is_empty(), |el| {
            el.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(6.0))
                    .bg(crate::theme::ink(0.05))
                    .font_family(theme.font_mono.clone())
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted)
                    .flex()
                    .flex_col()
                    .children(
                        output
                            .into_iter()
                            .map(|line| div().truncate().child(SharedString::from(line))),
                    ),
            )
        })
        .into_any_element()
}

/// The engine's install errors are a reason line followed by the
/// installer's output tail. Keep the last six lines of that output.
pub(crate) fn split_error(error: &str) -> (String, Vec<String>) {
    let error = error.trim();
    let (reason, rest) = error.split_once('\n').unwrap_or((error, ""));
    let lines: Vec<String> = rest
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(zeron_harness::install_progress::strip_ansi)
        .collect();
    let keep = lines.len().saturating_sub(6);
    (reason.to_string(), lines[keep..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_split_into_a_reason_and_the_last_six_lines() {
        let error = "Installation failed — installer exited with exit status: 1\n\
                     one\n\ntwo\nthree\nfour\nfive\nsix\n\u{1b}[31mseven\u{1b}[0m\n";
        let (reason, lines) = split_error(error);
        assert_eq!(
            reason,
            "Installation failed — installer exited with exit status: 1"
        );
        assert_eq!(lines, ["two", "three", "four", "five", "six", "seven"]);
        assert_eq!(split_error("no output"), ("no output".into(), vec![]));
    }

    #[test]
    fn percent_never_rounds_up_to_done() {
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.996), "99%");
        assert_eq!(percent(1.0), "100%");
    }
}
