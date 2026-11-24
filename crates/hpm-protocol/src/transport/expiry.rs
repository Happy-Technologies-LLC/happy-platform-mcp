//! Credential-expiry deadlines for long-lived streams (threat T-K02).
//!
//! A stream opened under a [`VerifiedHttpIdentity`] with
//! [`VerifiedHttpIdentity::expires_at`] ends at that time. The authenticator
//! folds its clock-skew allowance into `expires_at`; no skew is added here. A
//! later request on the same session by the same principal (the session binding
//! already refuses any other) refreshes the deadline. Identities without an
//! expiry (the single-user static bearer) never end a stream.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;

use axum::body::{Body, Bytes, HttpBody};
use axum::http::header;
use axum::response::Response;
use hpm_types::context::VerifiedHttpIdentity;
use hyper::body::{Frame, SizeHint};
use tokio::sync::watch;
use tokio::time::Instant;

fn deadline_of(identity: &VerifiedHttpIdentity) -> Option<Instant> {
    identity.expires_at.map(|exp| {
        let left = exp.duration_since(SystemTime::now()).unwrap_or_default();
        Instant::now() + left
    })
}

/// One session's (or one request's) stream deadline; `None` never fires.
#[derive(Clone)]
pub(crate) struct StreamDeadline(watch::Sender<Option<Instant>>);

impl StreamDeadline {
    pub(crate) fn new(identity: &VerifiedHttpIdentity) -> Self {
        Self(watch::Sender::new(deadline_of(identity)))
    }

    /// Applies a newer identity of the session's principal: the deadline moves to
    /// the later of the two expiries, and an identity without expiry lifts it.
    /// Callers must have checked the principal.
    pub(crate) fn refresh(&self, identity: &VerifiedHttpIdentity) {
        let next = deadline_of(identity);
        self.0.send_if_modified(|cur| {
            let new = match (*cur, next) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (_, None) | (None, _) => None,
            };
            let changed = new != *cur;
            *cur = new;
            changed
        });
    }

    /// Resolves when the current deadline passes; follows refreshes.
    pub(crate) fn expired(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.0.subscribe();
        async move {
            loop {
                let current = *rx.borrow_and_update();
                match current {
                    Some(at) => tokio::select! {
                        () = tokio::time::sleep_until(at) => return,
                        r = rx.changed() => if r.is_err() { std::future::pending::<()>().await },
                    },
                    None => {
                        if rx.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
        }
    }

    /// Ends `resp`'s body at the deadline when it is an event stream; other
    /// responses, and every response while no deadline is set (one is never
    /// added later), pass through unchanged.
    pub(crate) fn bound(&self, resp: Response) -> Response {
        let is_stream = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        if !is_stream || self.0.borrow().is_none() {
            return resp;
        }
        let (parts, body) = resp.into_parts();
        Response::from_parts(
            parts,
            Body::new(DeadlineBody {
                inner: body,
                expired: Box::pin(self.expired()),
            }),
        )
    }
}

/// A body that ends cleanly (end of stream, no error) once `expired` resolves.
struct DeadlineBody {
    inner: Body,
    expired: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl HttpBody for DeadlineBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if self.expired.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hpm_types::context::{ClientIdentity, Principal};

    use super::*;

    fn id(sub: &str, ttl: Option<Duration>) -> VerifiedHttpIdentity {
        let base = VerifiedHttpIdentity::new(
            Principal::Federated {
                org: "acme".into(),
                iss: "https://idp.example".into(),
                sub: sub.into(),
            },
            ClientIdentity {
                client_id: "c".into(),
                verified: true,
            },
            Vec::new().into(),
        );
        match ttl {
            Some(t) => base.with_expires_at(SystemTime::now() + t),
            None => base,
        }
    }

    async fn fired(d: &StreamDeadline, after: Duration) -> bool {
        tokio::time::timeout(after, d.expired()).await.is_ok()
    }

    #[tokio::test(start_paused = true)]
    async fn fires_at_expiry_and_refresh_extends() {
        let d = StreamDeadline::new(&id("alice", Some(Duration::from_secs(60))));
        assert!(!fired(&d, Duration::from_secs(30)).await);
        d.refresh(&id("alice", Some(Duration::from_secs(120))));
        // Old deadline (≈60s from start) has passed: still open.
        assert!(!fired(&d, Duration::from_secs(60)).await);
        assert!(fired(&d, Duration::from_secs(91)).await);
        // An older (shorter) token never shortens it.
        let d = StreamDeadline::new(&id("alice", Some(Duration::from_secs(60))));
        d.refresh(&id("alice", Some(Duration::from_secs(10))));
        assert!(!fired(&d, Duration::from_secs(30)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn no_expiry_never_fires() {
        let d = StreamDeadline::new(&id("alice", None));
        assert!(!fired(&d, Duration::from_secs(365 * 24 * 3600)).await);
    }
}
