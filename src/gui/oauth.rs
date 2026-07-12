//! Subscription sign-in from the browser.
//!
//! An API key is a string the user can paste; a subscription is not. It is an
//! OAuth round trip: we hand the browser an authorize URL, the provider sends
//! the user back to a redirect we serve, and we exchange the code for tokens
//! that live in `~/.wizard/`. The terminal flows ([`crate::llm::xai_oauth::login`])
//! bind a listener of their own for that redirect; the GUI already *is* a
//! server, so it serves the redirect itself — `/callback` on the same origin
//! the user is already looking at.
//!
//! One sign-in may be in flight at a time. That is not a limitation worth
//! engineering around: a person signs in to one account at a time, and a second
//! attempt should replace the first rather than race it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::gui::settings::ConfigStore;
use crate::llm::{chatgpt_oauth, xai_oauth};

/// A sign-in that has been started but not yet completed. Dropped when it is
/// finished, replaced, or [`EXPIRY`] passes — an authorization code the user
/// never came back with is not worth holding forever.
enum Flow {
    Xai(Box<xai_oauth::PendingLogin>),
}

/// How long a started sign-in stays valid. The provider's own code lifetime is
/// shorter than this; this bound only stops a forgotten flow from lingering.
const EXPIRY: Duration = Duration::from_secs(10 * 60);

/// What the frontend polls while the user is off in the provider's tab.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No sign-in has been started.
    Idle,
    /// Waiting for the user to finish in the provider's tab.
    Pending { provider: String },
    /// Signed in; the provider is configured and active.
    Done { provider: String },
    /// The exchange failed, or the user denied it.
    Failed { provider: String, error: String },
}

/// The one in-flight sign-in, plus the outcome of the last one.
pub struct SignIn {
    inner: Mutex<Inner>,
}

struct Inner {
    flow: Option<(Flow, Instant)>,
    status: Status,
}

impl Default for SignIn {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                flow: None,
                status: Status::Idle,
            }),
        }
    }
}

impl SignIn {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub fn status(&self) -> Status {
        let mut inner = self.lock();
        if let Some((_, started)) = &inner.flow
            && started.elapsed() > EXPIRY
        {
            inner.flow = None;
            inner.status = Status::Idle;
        }
        inner.status.clone()
    }

    /// Start an xAI sign-in against a redirect this server serves. The GUI's
    /// own `/callback` route finishes it via [`Self::complete`].
    pub async fn begin_xai(&self, redirect_uri: &str) -> anyhow::Result<String> {
        let pending = xai_oauth::begin_login(redirect_uri).await?;
        let url = pending.authorize_url.clone();
        let mut inner = self.lock();
        inner.flow = Some((Flow::Xai(Box::new(pending)), Instant::now()));
        inner.status = Status::Pending {
            provider: "xai".to_string(),
        };
        Ok(url)
    }

    /// Start a ChatGPT sign-in. Its redirect is a fixed address registered with
    /// OpenAI (`localhost:1455/auth/callback`), not this server's, so the flow
    /// binds that listener itself and finishes in a spawned task — the GUI only
    /// hands out the URL and then watches [`Status`]. `store` receives the
    /// provider once the tokens land.
    pub async fn begin_chatgpt(
        self: &Arc<Self>,
        store: Arc<ConfigStore>,
    ) -> anyhow::Result<String> {
        let pending = chatgpt_oauth::begin_login()?;
        let url = pending.authorize_url.clone();
        {
            let mut inner = self.lock();
            // A ChatGPT flow owns its own listener; nothing for `complete` to do.
            inner.flow = None;
            inner.status = Status::Pending {
                provider: "chatgpt".to_string(),
            };
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match chatgpt_oauth::wait_and_complete(pending).await {
                Ok(_) => {
                    let provider = chatgpt_oauth::provider_config();
                    let name = provider.name.clone();
                    let saved = store.update(move |config| {
                        crate::gui::settings::upsert_provider(config, provider, true);
                        Ok(())
                    });
                    match saved {
                        Ok(_) => this.finish(Status::Done { provider: name }),
                        Err(err) => this.finish(Status::Failed {
                            provider: "chatgpt".to_string(),
                            error: format!("signed in, but saving failed: {err:#}"),
                        }),
                    }
                }
                Err(err) => this.finish(Status::Failed {
                    provider: "chatgpt".to_string(),
                    error: format!("{err:#}"),
                }),
            }
        });
        Ok(url)
    }

    /// Take the in-flight sign-in, if there is one, for the callback to finish.
    fn take(&self) -> Option<Flow> {
        self.lock().flow.take().map(|(flow, _)| flow)
    }

    fn finish(&self, status: Status) {
        self.lock().status = status;
    }

    /// Complete whatever sign-in is in flight with the redirect's `code` and
    /// `state`, returning the provider to configure.
    ///
    /// The error is recorded as well as returned: the tab the user started from
    /// is polling [`Status`] and is the one that has to report it.
    pub async fn complete(
        &self,
        code: &str,
        state: &str,
    ) -> anyhow::Result<crate::config::ProviderConfig> {
        let Some(flow) = self.take() else {
            anyhow::bail!("no sign-in is in progress — start again from Settings");
        };
        match flow {
            Flow::Xai(pending) => {
                let provider = "xai".to_string();
                match xai_oauth::complete_login(*pending, code, state).await {
                    Ok(_) => {
                        self.finish(Status::Done {
                            provider: provider.clone(),
                        });
                        Ok(xai_oauth::provider_config())
                    }
                    Err(err) => {
                        let error = format!("{err:#}");
                        self.finish(Status::Failed { provider, error });
                        Err(err)
                    }
                }
            }
        }
    }

    /// The user denied the request, or the provider sent an error back.
    pub fn deny(&self, error: String) {
        let provider = match self.lock().flow.take() {
            Some((Flow::Xai(_), _)) => "xai".to_string(),
            None => "unknown".to_string(),
        };
        self.finish(Status::Failed { provider, error });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completing_with_no_flow_in_progress_is_an_error() {
        let sign_in = SignIn::default();
        assert_eq!(sign_in.status(), Status::Idle);
        assert!(sign_in.complete("code", "state").await.is_err());
    }

    #[test]
    fn a_denied_sign_in_is_reported_not_swallowed() {
        let sign_in = SignIn::default();
        sign_in.deny("access_denied".to_string());
        match sign_in.status() {
            Status::Failed { error, .. } => assert_eq!(error, "access_denied"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
