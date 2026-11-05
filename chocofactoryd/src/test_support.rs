//! Helpers for the timing-sensitive unit tests (#98).
//!
//! The wait helpers (`wait_until`, `LOAD_ALLOWANCE`, `POLL_INTERVAL`) live in
//! the dev-only `chocofactory-test-support` crate so `choco`'s tests can use
//! them too; they are re-exported here so `crate::test_support::…` paths work.

use std::time::Duration;

pub(crate) use chocofactory_test_support::{LOAD_ALLOWANCE, wait_until, write_executable};
// Kept so `crate::test_support::POLL_INTERVAL` still resolves; no caller in
// this crate uses it today.
#[allow(unused_imports)]
pub(crate) use chocofactory_test_support::POLL_INTERVAL;

/// For the few tests where a production timer must fire once and then must
/// *not* fire again before the fixture responds. Unlike `LOAD_ALLOWANCE`
/// this is a real upper bound on how slow the fixture may be, so it can't
/// be arbitrarily large without making the test slow; see the tests that
/// use it (#98).
pub(crate) const RESPONSE_MARGIN: Duration = Duration::from_secs(2);
