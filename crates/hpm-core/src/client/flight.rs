//! Keyed single-flight: concurrent callers for one key share the leader's
//! result, failures included (the shared-promise semantics of 6.x). A leader
//! that is cancelled frees the key so the next caller starts a fresh flight.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::watch;

use super::error::ClientError;

type Shared = Option<Result<String, ClientError>>;

#[derive(Default)]
pub(crate) struct Flights {
    slots: Mutex<HashMap<String, watch::Receiver<Shared>>>,
}

struct LeaderGuard<'a> {
    flights: &'a Flights,
    key: String,
    rx: watch::Receiver<Shared>,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        let mut slots = self.flights.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots
            .get(&self.key)
            .is_some_and(|rx| rx.same_channel(&self.rx))
        {
            slots.remove(&self.key);
        }
    }
}

enum Role {
    Lead(watch::Sender<Shared>, watch::Receiver<Shared>),
    Follow(watch::Receiver<Shared>),
}

impl Flights {
    pub(crate) async fn run<F>(
        &self,
        key: &str,
        make: impl FnOnce() -> F,
    ) -> Result<String, ClientError>
    where
        F: Future<Output = Result<String, ClientError>>,
    {
        let role = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            match slots.get(key) {
                Some(rx) => Role::Follow(rx.clone()),
                _ => {
                    let (tx, rx) = watch::channel(None);
                    slots.insert(key.to_owned(), rx.clone());
                    Role::Lead(tx, rx)
                }
            }
        };
        match role {
            Role::Follow(mut rx) => match rx.wait_for(Option::is_some).await {
                Ok(value) => value
                    .clone()
                    .unwrap_or_else(|| Err(ClientError::plain("Request was cancelled"))),
                Err(_) => Err(ClientError::plain("Request was cancelled")),
            },
            Role::Lead(tx, rx) => {
                let guard = LeaderGuard {
                    flights: self,
                    key: key.to_owned(),
                    rx,
                };
                let result = make().await;
                drop(guard);
                let _ = tx.send(Some(result.clone()));
                result
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn shares_success_and_failure() {
        let flights = Arc::new(Flights::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let run = |ok: bool| {
            let flights = flights.clone();
            let calls = calls.clone();
            async move {
                flights
                    .run("k", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        if ok {
                            Ok("v".to_owned())
                        } else {
                            Err(ClientError::plain("boom"))
                        }
                    })
                    .await
            }
        };
        let (a, b) = tokio::join!(run(false), run(true));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.err().map(|e| e.message), Some("boom".into()));
        assert_eq!(b.err().map(|e| e.message), Some("boom".into()));
        assert_eq!(run(true).await.ok().as_deref(), Some("v"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
