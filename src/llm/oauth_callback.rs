//! The loopback redirect a subscription sign-in comes back on.
//!
//! Both providers register one fixed loopback address with their client id —
//! OpenAI's `localhost:1455/auth/callback`, xAI's `127.0.0.1:56121/callback` —
//! and redirect the browser nowhere else. So every caller, terminal or GUI,
//! must own *that* port for the length of the flow; there is no other address
//! to serve the redirect on.
//!
//! Which makes the port a scarce resource, and the wait for it cancellable:
//! a sign-in that holds the port for its full [`CALLBACK_TIMEOUT`] would lock
//! out the most natural retry there is — close the provider tab, click sign in
//! again. So the accept loop races the browser against a [`Cancel`] signal, and
//! a cancelled flow drops the listener at once, leaving the port free for the
//! sign-in that replaced it.
//!
//! The two providers differ only in how they classify a request target (their
//! paths and error shapes differ); everything else — accepting, reading,
//! answering the human's browser with a page — is here, once.

use std::net::TcpListener as StdTcpListener;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// How long the listener waits for the browser before giving the port back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How long one connection has to send its request line. A browser sends it
/// immediately; anything else is not the redirect we are waiting for.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The request line fits well inside this; only the GET target is needed.
const MAX_REQUEST: usize = 8192;

/// Cancels the sign-in it was minted for, freeing the callback port. Dropping
/// it cancels too: a flow nobody holds a handle to is a flow nobody is waiting
/// for.
pub struct Canceller(oneshot::Sender<()>);

/// The receiving half of [`Canceller`], handed to [`serve_redirect`].
pub struct Cancel(Option<oneshot::Receiver<()>>);

/// A cancel handle and the signal it fires.
pub fn cancellation() -> (Canceller, Cancel) {
    let (tx, rx) = oneshot::channel();
    (Canceller(tx), Cancel(Some(rx)))
}

impl Canceller {
    /// Cancel the flow: it drops its listener and gives the port back.
    pub fn cancel(self) {
        let _ = self.0.send(());
    }
}

impl Cancel {
    /// A signal that never fires: the terminal flows own the port for their
    /// whole run, and nothing else in the process is competing for it.
    pub fn never() -> Self {
        Self(None)
    }

    /// Resolves when the flow is cancelled — either explicitly or because the
    /// [`Canceller`] was dropped.
    async fn triggered(&mut self) {
        match &mut self.0 {
            Some(rx) => {
                let _ = rx.await;
            }
            None => std::future::pending().await,
        }
    }
}

/// What one callback request amounted to. The provider modules classify their
/// own targets: the paths and the error shapes are theirs, the plumbing is not.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    /// The redirect, carrying an authorization code and a matching `state`.
    Code(String),
    /// The redirect carried an OAuth error, or a `state` that did not match.
    Failed(String),
    /// Some other request (favicon and friends): answer it and keep waiting.
    Ignored,
}

/// Serve the loopback redirect on `listener` until the browser brings the
/// authorization code back, the flow is cancelled, or [`CALLBACK_TIMEOUT`]
/// passes. The listener is dropped — and the port freed — the moment this
/// returns, whichever of the three it was.
///
/// `classify` maps a request target (`/callback?code=…&state=…`) onto a
/// [`Callback`], validating `state` as it goes.
pub async fn serve_redirect<F>(
    listener: StdTcpListener,
    mut cancel: Cancel,
    classify: F,
) -> Result<String>
where
    F: Fn(&str) -> Callback,
{
    listener
        .set_nonblocking(true)
        .context("configuring the callback listener")?;
    let listener = TcpListener::from_std(listener).context("arming the callback listener")?;
    let timeout = tokio::time::sleep(CALLBACK_TIMEOUT);
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            () = cancel.triggered() => bail!("the sign-in was replaced by a newer one"),
            () = &mut timeout => bail!("timed out waiting for the browser sign-in (5 minutes)"),
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting the OAuth callback connection")?;
                if let Some(outcome) = serve_connection(stream, &classify).await {
                    return outcome;
                }
            }
        }
    }
}

/// Serve one connection. `Some(result)` ends the wait; `None` keeps waiting.
async fn serve_connection<F>(mut stream: TcpStream, classify: &F) -> Option<Result<String>>
where
    F: Fn(&str) -> Callback,
{
    let target = match tokio::time::timeout(READ_TIMEOUT, read_target(&mut stream)).await {
        Ok(Some(target)) => target,
        // A connection that sends nothing (or nothing in time) is not the
        // redirect; it must not end the wait.
        _ => return None,
    };

    match classify(&target) {
        Callback::Ignored => {
            respond(&mut stream, "404 Not Found", "Not found.").await;
            None
        }
        Callback::Failed(message) => {
            respond(&mut stream, "200 OK", &format!("Sign-in failed: {message}")).await;
            Some(Err(anyhow::anyhow!(message)))
        }
        Callback::Code(code) => {
            respond(
                &mut stream,
                "200 OK",
                "Signed in to Wizard. You can close this tab.",
            )
            .await;
            Some(Ok(code))
        }
    }
}

/// The request line's target, e.g. `/callback?code=…&state=…`.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = [0u8; MAX_REQUEST];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(&mut buf[len..]).await {
            Ok(0) => break,
            Ok(n) => {
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&buf[..len]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))?;
    Some(target.to_string())
}

/// Answer the browser with a one-line page. The text can carry a provider's
/// error string — attacker-adjacent input, rendered into HTML — so it is
/// escaped.
async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Wizard</title>\
         <body style=\"background:#0c0c0e;color:#ececee;font:14px system-ui;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <p>{}</p>",
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Escape text rendered into the callback page's HTML.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A provider's registered callback port is a single machine-wide resource, and
/// unit tests run in parallel threads of one process. Tests that bind a real one
/// take this first, so they queue rather than fight over it.
#[cfg(test)]
pub(crate) fn serial_callback_port() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind a port the rest of the suite cannot take from us.
    ///
    /// Not `bind(0)`: that draws from the OS ephemeral range, which is exactly
    /// the range every other test's `bind(0)` draws from. A test that frees its
    /// port and then rebinds it — which is the whole point of
    /// `cancelling_frees_the_port_immediately` — can find the kernel handed it
    /// to a concurrent test in between, and fail on a race that says nothing
    /// about the code. So we walk a private range below the ephemeral one,
    /// where no `bind(0)` can land.
    fn bind_private() -> (StdTcpListener, u16) {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(21_000);
        for _ in 0..1_000 {
            let port = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(port < 30_000, "ran out of private test ports");
            if let Ok(listener) = StdTcpListener::bind(("127.0.0.1", port)) {
                return (listener, port);
            }
        }
        panic!("no free port in the private test range");
    }

    /// Play the browser: send the redirect and read the page back. Async on
    /// purpose — a blocking client on the test's runtime thread would starve the
    /// server it is talking to, and the two would sit there until the timeout.
    async fn request(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut page = String::new();
        stream.read_to_string(&mut page).await.expect("read");
        page
    }

    /// The redirect: everything before the code arrives is plumbing.
    #[tokio::test]
    async fn the_browsers_redirect_yields_the_code() {
        let (listener, port) = bind_private();
        let served = tokio::spawn(serve_redirect(
            listener,
            Cancel::never(),
            |target| match target.split_once("code=") {
                Some((_, code)) => Callback::Code(code.to_string()),
                None => Callback::Ignored,
            },
        ));

        let page = request(port, "/callback?code=abc").await;

        assert_eq!(served.await.expect("join").expect("code"), "abc");
        assert!(page.contains("Signed in to Wizard"), "{page}");
    }

    /// The regression the GUI needs: a cancelled flow gives the port back at
    /// once, rather than sitting on it for the full timeout.
    #[tokio::test]
    async fn cancelling_frees_the_port_immediately() {
        let (listener, port) = bind_private();
        let (canceller, cancel) = cancellation();
        let served = tokio::spawn(serve_redirect(listener, cancel, |_| Callback::Ignored));
        // The listener is live: the port cannot be taken from under it.
        assert!(StdTcpListener::bind(("127.0.0.1", port)).is_err());

        canceller.cancel();
        let err = served.await.expect("join").expect_err("cancelled");
        assert!(err.to_string().contains("replaced"), "{err}");

        // And now the port comes back. The task's future — and the listener it
        // owns — is dropped before its `JoinHandle` resolves, but the kernel
        // does not always have the socket torn down by the time the very next
        // syscall asks for the port back, so the bind is retried briefly rather
        // than demanded on the first try. The claim under test is unharmed: the
        // point is that the port returns in milliseconds instead of being held
        // for CALLBACK_TIMEOUT, and a whole second is still three hundred times
        // short of that.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match StdTcpListener::bind(("127.0.0.1", port)) {
                Ok(_) => break,
                Err(err) if std::time::Instant::now() < deadline => {
                    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("the port was never given back: {err}"),
            }
        }
    }

    /// Dropping the handle is as good as cancelling: a flow nobody holds is a
    /// flow nobody is waiting for.
    #[tokio::test]
    async fn dropping_the_canceller_cancels() {
        let (listener, _) = bind_private();
        let (canceller, cancel) = cancellation();
        let served = tokio::spawn(serve_redirect(listener, cancel, |_| Callback::Ignored));
        drop(canceller);
        assert!(served.await.expect("join").is_err());
    }

    /// A provider's error text lands in a page a human is looking at.
    #[test]
    fn provider_text_is_escaped_into_the_page() {
        assert_eq!(html_escape("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
        assert!(html_escape("<script>alert(1)</script>").contains("&lt;script&gt;"));
    }
}
