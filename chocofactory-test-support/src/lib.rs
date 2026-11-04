//! Wait helpers shared by the timing-sensitive tests of `choco` and
//! `chocofactoryd` (#98). Dev-dependency only: nothing here enters a
//! shipped binary.

use std::time::Duration;

/// How long a test waits for something the code under test does
/// asynchronously (a subprocess starting, an event being written, a stage
/// being entered) before declaring it never happened.
///
/// This is a load allowance, not a deadline, and nothing asserts against
/// it: a wait returns the moment its condition holds, so a passing test is
/// no slower for it being large. It only has to outlast the worst
/// scheduling delay a loaded machine produces, since `cargo test` runs the
/// suite in parallel and often alongside other builds (#98).
pub const LOAD_ALLOWANCE: Duration = Duration::from_secs(30);

/// How often a wait re-checks its condition.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Re-runs `check` every `POLL_INTERVAL` until it returns `Ok`, and returns
/// that value. `Err(last_seen)` means "not yet", and carries a description
/// of what the check saw this time. Panics after `LOAD_ALLOWANCE` with
/// "timed out after {LOAD_ALLOWANCE:?} waiting for {what}; last saw: {last_seen}".
///
/// A check that reads a database, a file or a process must panic on a
/// failed read and return `Err` only for "not yet", so a broken read can't
/// look like a timeout.
pub async fn wait_until<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let deadline = tokio::time::Instant::now() + LOAD_ALLOWANCE;
    loop {
        let last_seen = match check().await {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {LOAD_ALLOWANCE:?} waiting for {what}; last saw: {last_seen}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `wait_until` for sync tests: the same contract, constants and panic
/// text, blocking the thread between checks. Never call it from an async
/// test; it would block the runtime.
pub fn wait_until_blocking<T>(what: &str, check: impl FnMut() -> Result<T, String>) -> T {
    wait_until_blocking_within(LOAD_ALLOWANCE, what, check)
}

/// `wait_until_blocking` with a caller-chosen bound, for a wait whose
/// worst case is known to need more than `LOAD_ALLOWANCE`. The panic text
/// names `allowance`.
pub fn wait_until_blocking_within<T>(
    allowance: Duration,
    what: &str,
    mut check: impl FnMut() -> Result<T, String>,
) -> T {
    let deadline = std::time::Instant::now() + allowance;
    loop {
        let last_seen = match check() {
            Ok(value) => return value,
            Err(seen) => seen,
        };
        if std::time::Instant::now() >= deadline {
            panic!("timed out after {allowance:?} waiting for {what}; last saw: {last_seen}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panic_message(err: Box<dyn std::any::Any + Send>) -> String {
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn returns_the_first_ok_without_sleeping() {
        let start = tokio::time::Instant::now();
        let v = wait_until("x", || async { Ok::<_, String>(7) }).await;
        assert_eq!(v, 7);
        assert_eq!(tokio::time::Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn returns_the_value_from_the_call_that_turned_ok() {
        let mut calls = 0;
        let v = wait_until("x", || {
            calls += 1;
            let n = calls;
            async move {
                if n == 3 {
                    Ok(n)
                } else {
                    Err(format!("call {n}"))
                }
            }
        })
        .await;
        assert_eq!(v, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn panics_after_the_load_allowance_naming_what_and_last_seen() {
        let start = tokio::time::Instant::now();
        let handle = tokio::spawn(async {
            let _: () = wait_until("the thing", || async { Err("still no".to_string()) }).await;
        });
        let err = handle.await.unwrap_err();
        assert!(err.is_panic());
        let msg = panic_message(err.into_panic());
        assert!(
            msg.contains("waiting for the thing; last saw: still no"),
            "{msg}"
        );
        assert!(tokio::time::Instant::now() - start >= LOAD_ALLOWANCE);
    }

    #[test]
    fn blocking_returns_the_first_ok_after_one_call() {
        let mut calls = 0;
        let v = wait_until_blocking("x", || {
            calls += 1;
            Ok::<_, String>(7)
        });
        assert_eq!((v, calls), (7, 1));
    }

    #[test]
    fn blocking_returns_the_value_from_the_call_that_turned_ok() {
        let mut calls = 0;
        let v = wait_until_blocking("x", || {
            calls += 1;
            if calls == 3 {
                Ok(calls)
            } else {
                Err(format!("call {calls}"))
            }
        });
        assert_eq!(v, 3);
    }

    #[test]
    fn blocking_panics_after_the_allowance_naming_what_and_last_seen() {
        let err = std::panic::catch_unwind(|| {
            let _: () = wait_until_blocking_within(Duration::from_millis(50), "the thing", || {
                Err("still no".to_string())
            });
        })
        .unwrap_err();
        let msg = panic_message(err);
        assert!(
            msg.contains("waiting for the thing; last saw: still no"),
            "{msg}"
        );
    }
}
