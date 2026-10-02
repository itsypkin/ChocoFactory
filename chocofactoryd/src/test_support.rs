//! Shared helpers for the timing-sensitive unit tests (#98).

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
pub(crate) const LOAD_ALLOWANCE: Duration = Duration::from_secs(30);

/// How often a wait re-checks its condition.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// For the few tests where a production timer must fire once and then must
/// *not* fire again before the fixture responds. Unlike `LOAD_ALLOWANCE`
/// this is a real upper bound on how slow the fixture may be, so it can't
/// be arbitrarily large without making the test slow; see the tests that
/// use it (#98).
pub(crate) const RESPONSE_MARGIN: Duration = Duration::from_secs(2);

/// Re-runs `check` every `POLL_INTERVAL` until it returns `Ok`, and returns
/// that value. `Err(last_seen)` means "not yet", and carries a description
/// of what the check saw this time. Panics after `LOAD_ALLOWANCE` with
/// "timed out after {LOAD_ALLOWANCE:?} waiting for {what}; last saw: {last_seen}".
pub(crate) async fn wait_until<T, F, Fut>(what: &str, mut check: F) -> T
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

#[cfg(test)]
mod tests {
    use super::*;

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
    #[should_panic(expected = "waiting for the thing; last saw: still no")]
    async fn panics_after_the_load_allowance_naming_what_and_last_seen() {
        let _: () = wait_until("the thing", || async { Err("still no".to_string()) }).await;
    }
}
