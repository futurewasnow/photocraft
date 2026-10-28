//! Bound read-only printer discovery without spawning more workers for a stuck backend.
use crate::Result;
use serde_json::Value;

pub(super) enum Kind {
    System,
    Rgb,
}

pub(super) fn run(kind: Kind, action: impl FnOnce() -> Result<Value> + Send + 'static) -> Result<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::{sync::OnceLock, time::Duration};
        static SYSTEM: OnceLock<native::Slot> = OnceLock::new();
        static RGB: OnceLock<native::Slot> = OnceLock::new();
        let slot = match kind {
            Kind::System => SYSTEM.get_or_init(native::Slot::default),
            Kind::Rgb => RGB.get_or_init(native::Slot::default),
        };
        slot.start(move || action().map_err(|error| error.to_string())).wait(Duration::from_secs(2)).map_err(crate::EngineError::Other)
    }
    #[cfg(target_arch = "wasm32")]
    {
        // The platform's wasm printer API returns Unsupported without contacting a service.
        let _ = kind;
        action()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use serde_json::Value;
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::atomic::{AtomicBool, Ordering},
        sync::{Arc, Condvar, Mutex, PoisonError},
        time::{Duration, Instant},
    };

    type Reply = std::result::Result<Value, String>;

    #[derive(Default)]
    pub(super) struct Slot {
        current: Mutex<Option<Arc<Flight>>>,
    }

    #[derive(Default)]
    pub(super) struct Flight {
        reply: Mutex<Option<(Instant, Reply)>>,
        ready: Condvar,
        delivered: AtomicBool,
    }

    impl Slot {
        pub(super) fn start(&self, action: impl FnOnce() -> Reply + Send + 'static) -> Arc<Flight> {
            let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(flight) = current.as_ref() {
                let reply = flight.reply.lock().unwrap_or_else(PoisonError::into_inner);
                if reply.as_ref().is_none_or(|(finished, _)| !flight.delivered.load(Ordering::Acquire) && finished.elapsed() < Duration::from_secs(30)) {
                    return Arc::clone(flight);
                }
            }
            // Refresh delivered or stale replies: printer lists and environment may change.
            // Retain an undelivered completion briefly so retrying a timed-out query can
            // retrieve its result rather than starting another equally slow query forever.
            // Existing waiters retain their own Arc and still receive the previous reply.
            let flight = Arc::new(Flight::default());
            *current = Some(Arc::clone(&flight));
            let worker = Arc::clone(&flight);
            if let Err(error) = std::thread::Builder::new().name("printer-discovery".into()).spawn(move || {
                let reply = catch_unwind(AssertUnwindSafe(action)).unwrap_or_else(|_| Err("printer discovery worker failed".into()));
                worker.finish(reply);
            }) {
                flight.finish(Err(format!("could not start printer discovery: {error}")));
            }
            flight
        }
    }

    impl Flight {
        fn finish(&self, value: Reply) {
            *self.reply.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), value));
            self.ready.notify_all();
        }

        pub(super) fn wait(&self, timeout: Duration) -> Reply {
            let started = Instant::now();
            let mut reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if let Some((_, value)) = reply.as_ref() {
                    self.delivered.store(true, Ordering::Release);
                    return value.clone();
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err("printer discovery timed out; the system service is still busy, try again later".into());
                }
                let (next, _) = self.ready.wait_timeout(reply, remaining).unwrap_or_else(PoisonError::into_inner);
                reply = next;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        #[test]
        fn timeout_reuses_one_pending_worker_and_preserves_its_result() {
            let slot = Slot::default();
            let calls = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&calls);
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let first = slot.start(move || {
                count.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).map_err(|error| error.to_string())?;
                release_rx.recv().map_err(|error| error.to_string())?;
                Ok(json!({"printers":[{"name":"Synthetic"}]}))
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(first.wait(Duration::from_millis(5)).unwrap_err().contains("timed out"));
            for _ in 0..4 {
                let count = Arc::clone(&calls);
                let next = slot.start(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(json!("unexpected extra worker"))
                });
                assert!(Arc::ptr_eq(&first, &next));
                assert!(next.wait(Duration::from_millis(5)).is_err());
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            release_tx.send(()).unwrap();
            assert_eq!(first.wait(Duration::from_secs(5)).unwrap()["printers"][0]["name"], "Synthetic");
        }

        #[test]
        fn completed_queries_refresh_and_existing_waiters_keep_their_reply() {
            let slot = Slot::default();
            let first = slot.start(|| Ok(json!(["First"])));
            assert_eq!(first.wait(Duration::from_secs(5)).unwrap(), json!(["First"]));
            let second = slot.start(|| Ok(json!(["Second"])));
            assert!(!Arc::ptr_eq(&first, &second));
            assert_eq!(second.wait(Duration::from_secs(5)).unwrap(), json!(["Second"]));
            assert_eq!(first.wait(Duration::ZERO).unwrap(), json!(["First"]));
        }

        #[test]
        fn backend_failure_is_not_reported_as_an_empty_printer_list() {
            let slot = Slot::default();
            assert_eq!(slot.start(|| Err("synthetic backend failure".into())).wait(Duration::from_secs(5)).unwrap_err(), "synthetic backend failure");
        }

        #[test]
        fn retry_can_read_a_completion_after_the_first_caller_timed_out() {
            let slot = Slot::default();
            let pending = Arc::new(Flight::default());
            *slot.current.lock().unwrap() = Some(Arc::clone(&pending));
            assert!(pending.wait(Duration::ZERO).is_err());
            pending.finish(Ok(json!(["Late result"])));
            let retry = slot.start(|| Err("must reuse undelivered reply".into()));
            assert!(Arc::ptr_eq(&pending, &retry));
            assert_eq!(retry.wait(Duration::ZERO).unwrap(), json!(["Late result"]));
        }

        #[test]
        fn an_unobserved_but_stale_result_is_refreshed() {
            let slot = Slot::default();
            let old = Arc::new(Flight::default());
            *old.reply.lock().unwrap() = Some((Instant::now() - Duration::from_secs(31), Ok(json!(["Stale"]))));
            *slot.current.lock().unwrap() = Some(Arc::clone(&old));
            let fresh = slot.start(|| Ok(json!(["Fresh"])));
            assert!(!Arc::ptr_eq(&old, &fresh));
            assert_eq!(fresh.wait(Duration::from_secs(5)).unwrap(), json!(["Fresh"]));
        }
    }
}
