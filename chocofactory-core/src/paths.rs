//! Where chocofactory's user-owned state lives on disk.

use std::path::PathBuf;

/// `$HOME/.config/chocofactory`, or `None` if `$HOME` isn't set. Callers
/// either fall back to an explicit path (tests) or accept that no default
/// exists (e.g. no global config file to load) rather than failing
/// startup just because `$HOME` is unset.
pub fn config_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("chocofactory"))
}
