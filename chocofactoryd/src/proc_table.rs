//! The process table, and the rule for which processes a single-shot turn
//! started.
//!
//! Claude Code runs every Bash tool call in a new session of its own, so
//! killing the agent's process group leaves a turn's commands running (a hung
//! `cargo test`, a dev server, a `sleep`). When a turn ends the daemon
//! therefore scans the table, decides which processes it can *prove* the turn
//! started ([`owned_pids`]), and SIGKILLs those by pid.
//!
//! # Why the rule proves ownership
//!
//! Inputs: the table, the agent pid `A` (while it is not yet reaped), the
//! turn's marker `M`, the identities `R` recorded earlier, the daemon's pid
//! `D` and session id `DS`.
//!
//! Never owned, whatever else holds: pid <= 1, `D`, zombies, other uids, and
//! `A` itself (its group is killed by `killpg`).
//!
//! Seeds, owned if any holds:
//! - (a) a descendant of `A` now, following ppid links where the child
//!   started no earlier than its parent. `A` is the daemon's unreaped child,
//!   so its pid cannot be reused and a ppid always names the live parent;
//!   the start-time check guards a pid reused mid-scan.
//! - (b) its `(pid, start time)` is in `R`. An identity is recorded only for
//!   a process proven owned by (a) at that time; the start time makes a
//!   reused pid fail the match.
//! - (c) its environment holds `M=`. `M` is random per spawn and only `A`'s
//!   environment has it, so a process carrying it descends from `A`.
//!
//! Session closure: a process is also owned if its session id equals a
//! seed's, unless that id is `DS`, `0` or `1` (or the agent's own session).
//! The agent shares the daemon's session (`process_group(0)` does not create
//! one), hence the `DS` exclusion. A seed in any other session `S` got `S`
//! from a `setsid` by itself or an ancestor below `A`, so the session's
//! creator is owned, and every process in `S` descends from that creator
//! because nothing can join an existing session. Seed and members come from
//! the same scan, so `S` cannot have been freed and reused in between.
//! Process *groups* are deliberately not closed over: `setpgid` can move a
//! process into another existing group of the same session.
//!
//! # Blind spots (accepted)
//!
//! - macOS hides the environment of Apple platform binaries (`/bin/zsh`,
//!   `/bin/sleep`, `/usr/bin/perl`). One that was never a descendant of a
//!   live `A` when recorded and shares no session with an owned process (a
//!   foreground `nohup /bin/sleep 999 &`, alone in its call) survives.
//! - Linux: `/proc/<pid>/environ` is unreadable for non-dumpable processes.
//! - A process that drops its environment and leaves both the tree and the
//!   session before it is recorded.
//! - Processes started through a service manager, and processes of another
//!   uid.
//! - pid reuse between a scan and the kill that follows is not guarded: both
//!   kernels allocate pids sequentially, so it needs a full wraparound in a
//!   window of microseconds.

use std::collections::{HashMap, HashSet};
use std::io;

/// Whether a process's environment holds the turn's marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerStatus {
    Present,
    Absent,
    /// The environment could not be read.
    Unreadable,
    /// No marker was asked for.
    NotChecked,
}

/// One process of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub sid: i32,
    pub uid: u32,
    /// Start time, comparable only between entries of one platform: microseconds
    /// since the epoch on macOS, clock ticks since boot on Linux.
    pub start: u64,
    pub zombie: bool,
    /// A short command name.
    pub comm: String,
    pub marker: MarkerStatus,
}

/// `(pid, start time)`: a process identity that survives pid reuse.
pub type Identity = (i32, u64);

/// A table reader; swapped in tests.
pub type ProcReader =
    std::sync::Arc<dyn Fn(Option<&str>) -> io::Result<Vec<ProcEntry>> + Send + Sync>;

/// The facts the ownership rule needs besides the table.
pub struct OwnershipInput<'a> {
    pub agent: Option<i32>,
    pub recorded: &'a HashSet<Identity>,
    pub daemon_pid: i32,
    pub daemon_sid: i32,
    pub uid: u32,
}

fn never_owned(entry: &ProcEntry, input: &OwnershipInput<'_>) -> bool {
    entry.pid <= 1
        || entry.pid == input.daemon_pid
        || entry.zombie
        || entry.uid != input.uid
        || Some(entry.pid) == input.agent
}

/// Current descendants of the agent (rule (a)), as pids.
pub fn descendants(table: &[ProcEntry], agent: i32) -> Vec<i32> {
    let by_pid: HashMap<i32, &ProcEntry> = table.iter().map(|e| (e.pid, e)).collect();
    let mut children: HashMap<i32, Vec<&ProcEntry>> = HashMap::new();
    for entry in table {
        children.entry(entry.ppid).or_default().push(entry);
    }
    let mut found = Vec::new();
    let mut seen: HashSet<i32> = HashSet::from([agent]);
    let mut queue = vec![agent];
    while let Some(parent) = queue.pop() {
        let parent_start = by_pid.get(&parent).map(|p| p.start);
        for child in children.get(&parent).into_iter().flatten() {
            // A child that started before its parent cannot be its child: the
            // parent's pid was reused while the table was being read.
            if parent_start.is_some_and(|start| child.start < start) {
                continue;
            }
            if seen.insert(child.pid) {
                found.push(child.pid);
                queue.push(child.pid);
            }
        }
    }
    found
}

/// The processes the table shows that the turn provably started. See the
/// module docs for the rule.
pub fn owned_pids(table: &[ProcEntry], input: &OwnershipInput<'_>) -> Vec<i32> {
    let by_pid: HashMap<i32, &ProcEntry> = table.iter().map(|e| (e.pid, e)).collect();
    let mut seeds: HashSet<i32> = HashSet::new();
    if let Some(agent) = input.agent {
        seeds.extend(descendants(table, agent));
    }
    for entry in table {
        if input.recorded.contains(&(entry.pid, entry.start))
            || entry.marker == MarkerStatus::Present
        {
            seeds.insert(entry.pid);
        }
    }
    seeds.retain(|pid| by_pid.get(pid).is_some_and(|e| !never_owned(e, input)));

    let agent_sid = input.agent.and_then(|a| by_pid.get(&a)).map(|a| a.sid);
    let sessions: HashSet<i32> = seeds
        .iter()
        .map(|pid| by_pid[pid].sid)
        .filter(|sid| ![0, 1, input.daemon_sid].contains(sid) && Some(*sid) != agent_sid)
        .collect();

    let mut owned: Vec<i32> = table
        .iter()
        .filter(|e| !never_owned(e, input))
        .filter(|e| seeds.contains(&e.pid) || sessions.contains(&e.sid))
        .map(|e| e.pid)
        .collect();
    owned.sort_unstable();
    owned
}

/// What `kill(pid, SIGKILL)` did.
#[derive(Debug)]
pub enum PidKill {
    /// The signal was delivered to a live process.
    Killed,
    /// The process was already gone (`ESRCH`): not an error, not a kill.
    Gone,
    Failed(io::Error),
}

/// SIGKILLs one process by pid.
pub fn kill_pid(pid: i32) -> PidKill {
    if pid <= 1 {
        return PidKill::Gone;
    }
    // SAFETY: `kill` only sends a signal; failures come back in the result.
    let result = unsafe { libc::kill(pid, libc::SIGKILL) };
    if result == 0 {
        return PidKill::Killed;
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        PidKill::Gone
    } else {
        PidKill::Failed(err)
    }
}

/// Reads the table: one entry per process of the daemon's own uid.
///
/// `marker` is the full variable name to look for in each environment, or
/// `None` to read no environments (cheap). A process that vanishes mid-read
/// is skipped; failing to list processes at all is an `Err`.
///
/// Blocking: callers on a runtime thread use `spawn_blocking`.
pub fn read(marker: Option<&str>) -> io::Result<Vec<ProcEntry>> {
    platform::read(marker)
}

/// Fills a pid buffer through `list`, which writes pids into the slice and
/// returns how many it wrote. A result that fills the buffer may have been
/// cut short, so the buffer doubles and the call repeats until it does not.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn fill_pids(
    initial: usize,
    mut list: impl FnMut(&mut [i32]) -> io::Result<usize>,
) -> io::Result<Vec<i32>> {
    let mut capacity = initial.max(1);
    loop {
        let mut pids = vec![0i32; capacity];
        let count = list(&mut pids)?;
        if count < capacity {
            pids.truncate(count);
            return Ok(pids);
        }
        capacity = capacity.checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::OutOfMemory, "the pid list never fit a buffer")
        })?;
    }
}

/// Whether an environment entry list holds `<marker>=`.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn env_has_marker<'a>(mut entries: impl Iterator<Item = &'a [u8]>, marker: &str) -> bool {
    let prefix = format!("{marker}=");
    entries.any(|entry| entry.starts_with(prefix.as_bytes()))
}

/// Parses `/proc/<pid>/stat`: (state, ppid, pgrp, session, starttime, comm).
/// Fields are counted after the *last* `)`, since the command name may hold
/// parentheses and spaces.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_linux_stat(stat: &str) -> Option<(char, i32, i32, i32, u64, String)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    // After ")": state ppid pgrp session ... (field 3 onwards).
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let state = rest.first()?.chars().next()?;
    let ppid = rest.get(1)?.parse().ok()?;
    let pgrp = rest.get(2)?.parse().ok()?;
    let session = rest.get(3)?.parse().ok()?;
    // Field 22 is index 19 here (field 3 is index 0).
    let start = rest.get(19)?.parse().ok()?;
    Some((state, ppid, pgrp, session, start, comm))
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub fn read(marker: Option<&str>) -> io::Result<Vec<ProcEntry>> {
        // SAFETY: `geteuid` has no preconditions.
        let me = unsafe { libc::geteuid() };
        let mut table = Vec::new();
        for dirent in std::fs::read_dir("/proc")? {
            let Ok(dirent) = dirent else { continue };
            let Some(pid) = dirent
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i32>().ok())
            else {
                continue;
            };
            let dir = format!("/proc/{pid}");
            let Ok(meta) = std::fs::metadata(&dir) else {
                continue;
            };
            if meta.uid() != me {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(format!("{dir}/stat")) else {
                continue;
            };
            let Some((state, ppid, pgid, sid, start, comm)) = parse_linux_stat(&stat) else {
                continue;
            };
            let zombie = state == 'Z' || state == 'X';
            let marker_status = match marker {
                None => MarkerStatus::NotChecked,
                Some(_) if zombie => MarkerStatus::Absent,
                Some(name) => match std::fs::read(format!("{dir}/environ")) {
                    Ok(environ) => {
                        if env_has_marker(environ.split(|b| *b == 0), name) {
                            MarkerStatus::Present
                        } else {
                            MarkerStatus::Absent
                        }
                    }
                    Err(_) => MarkerStatus::Unreadable,
                },
            };
            table.push(ProcEntry {
                pid,
                ppid,
                pgid,
                sid,
                uid: meta.uid(),
                start,
                zombie,
                comm,
                marker: marker_status,
            });
        }
        Ok(table)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// Pids of every process, growing the buffer until the kernel's answer
    /// fits with room to spare (a full buffer may be a truncated list).
    fn list_pids(initial: usize) -> io::Result<Vec<i32>> {
        fill_pids(initial, |buf| {
            // SAFETY: the buffer is `buf.len() * 4` bytes long.
            let count = unsafe {
                libc::proc_listallpids(
                    buf.as_mut_ptr().cast(),
                    (buf.len() * std::mem::size_of::<i32>()) as libc::c_int,
                )
            };
            if count <= 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
    }

    fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: all-zero is a valid `proc_bsdinfo` (plain integers).
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is `size` bytes of writable memory.
        let got = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        // 0 also for an unreaped zombie, which is never owned anyway.
        (got == size).then_some(info)
    }

    /// The process's environment entries, or `None` when the OS hides them.
    fn environment(pid: i32) -> Option<Vec<Vec<u8>>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks for the required size.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size + 4096];
        let mut len: libc::size_t = buf.len();
        // SAFETY: `buf` has `len` writable bytes.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(len);
        parse_procargs2(&buf)
    }

    /// `argc`, the exec path, padding NULs, `argc` arguments, then the
    /// environment, all NUL-separated.
    pub(super) fn parse_procargs2(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
        let mut parts = buf.get(4..)?.split(|b| *b == 0);
        // Exec path.
        parts.next()?;
        // Padding is empty segments; the first non-empty one is argv[0].
        let mut rest = parts.skip_while(|p| p.is_empty());
        for _ in 0..argc.max(0) {
            rest.next()?;
        }
        Some(rest.filter(|p| !p.is_empty()).map(<[u8]>::to_vec).collect())
    }

    pub fn read(marker: Option<&str>) -> io::Result<Vec<ProcEntry>> {
        read_with_capacity(marker, 1024)
    }

    /// `read` with a chosen starting size for the pid buffer, so a test can
    /// start below the live count.
    pub fn read_with_capacity(
        marker: Option<&str>,
        initial_pids: usize,
    ) -> io::Result<Vec<ProcEntry>> {
        // SAFETY: `geteuid` has no preconditions.
        let me = unsafe { libc::geteuid() };
        let mut table = Vec::new();
        for pid in list_pids(initial_pids)? {
            if pid <= 0 {
                continue;
            }
            let Some(info) = bsd_info(pid) else { continue };
            if info.pbi_uid != me {
                continue;
            }
            // SAFETY: `getsid` only reads kernel state.
            let sid = unsafe { libc::getsid(pid) };
            if sid < 0 {
                continue;
            }
            let zombie = info.pbi_status == libc::SZOMB;
            let marker_status = match marker {
                None => MarkerStatus::NotChecked,
                Some(_) if zombie => MarkerStatus::Absent,
                Some(name) => match environment(pid) {
                    Some(env) => {
                        if env_has_marker(env.iter().map(Vec::as_slice), name) {
                            MarkerStatus::Present
                        } else {
                            MarkerStatus::Absent
                        }
                    }
                    None => MarkerStatus::Unreadable,
                },
            };
            let comm_bytes: Vec<u8> = info
                .pbi_comm
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            table.push(ProcEntry {
                pid,
                ppid: info.pbi_ppid as i32,
                pgid: info.pbi_pgid as i32,
                sid,
                uid: info.pbi_uid,
                start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
                zombie,
                comm: String::from_utf8_lossy(&comm_bytes).into_owned(),
                marker: marker_status,
            });
        }
        Ok(table)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub fn read(_marker: Option<&str>) -> io::Result<Vec<ProcEntry>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reading the process table is not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i32 = 100;
    const DS: i32 = 50;
    const A: i32 = 200;

    fn p(pid: i32, ppid: i32, sid: i32, start: u64) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            pgid: pid,
            sid,
            uid: 501,
            start,
            zombie: false,
            comm: format!("p{pid}"),
            marker: MarkerStatus::Absent,
        }
    }

    fn input(recorded: &HashSet<Identity>) -> OwnershipInput<'_> {
        OwnershipInput {
            agent: Some(A),
            recorded,
            daemon_pid: D,
            daemon_sid: DS,
            uid: 501,
        }
    }

    fn base() -> Vec<ProcEntry> {
        vec![p(D, 1, DS, 10), p(A, D, DS, 20)]
    }

    #[test]
    fn a_chain_through_a_setsid_child_and_its_session_is_owned() {
        let mut table = base();
        // Child of the agent that leads a new session.
        table.push(p(300, A, 300, 30));
        // Its grandchild, whose parent died: ppid 1, same session.
        table.push(p(301, 1, 300, 31));
        // Unrelated, in another session.
        table.push(p(400, 1, 400, 32));
        let none = HashSet::new();
        assert_eq!(owned_pids(&table, &input(&none)), vec![300, 301]);
    }

    #[test]
    fn a_reparented_orphan_is_owned_only_with_the_marker_or_a_record() {
        let mut table = base();
        let mut orphan = p(300, 1, 300, 30);
        let none = HashSet::new();
        assert!(
            owned_pids(
                &[table.clone(), vec![orphan.clone()]].concat(),
                &input(&none)
            )
            .is_empty()
        );
        orphan.marker = MarkerStatus::Present;
        table.push(orphan);
        assert_eq!(owned_pids(&table, &input(&none)), vec![300]);
    }

    #[test]
    fn a_recorded_identity_with_another_start_time_is_not_owned() {
        let mut table = base();
        table.push(p(300, 1, 300, 30));
        let same: HashSet<Identity> = HashSet::from([(300, 30)]);
        assert_eq!(owned_pids(&table, &input(&same)), vec![300]);
        let reused: HashSet<Identity> = HashSet::from([(300, 29)]);
        assert!(owned_pids(&table, &input(&reused)).is_empty());
    }

    #[test]
    fn session_closure_never_applies_to_the_daemons_session_zero_or_one() {
        let mut table = base();
        // A seed in the daemon's own session, plus a bystander there.
        let mut seed = p(300, 1, DS, 30);
        seed.marker = MarkerStatus::Present;
        table.push(seed);
        table.push(p(301, 1, DS, 31));
        // Seeds in sessions 0 and 1 with bystanders.
        let mut seed0 = p(310, 1, 0, 30);
        seed0.marker = MarkerStatus::Present;
        table.push(seed0);
        table.push(p(311, 1, 0, 31));
        let mut seed1 = p(320, 1, 1, 30);
        seed1.marker = MarkerStatus::Present;
        table.push(seed1);
        table.push(p(321, 1, 1, 31));
        let none = HashSet::new();
        assert_eq!(owned_pids(&table, &input(&none)), vec![300, 310, 320]);
    }

    #[test]
    fn some_processes_are_never_owned() {
        let mut table = base();
        table[0].marker = MarkerStatus::Present; // the daemon
        table[1].marker = MarkerStatus::Present; // the agent
        let mut init = p(1, 0, 1, 1);
        init.marker = MarkerStatus::Present;
        let mut zero = p(0, 0, 0, 1);
        zero.marker = MarkerStatus::Present;
        let mut zombie = p(300, A, 300, 30);
        zombie.zombie = true;
        let mut stranger = p(301, A, 301, 30);
        stranger.uid = 502;
        table.extend([init, zero, zombie, stranger]);
        let recorded: HashSet<Identity> = table.iter().map(|e| (e.pid, e.start)).collect();
        assert!(owned_pids(&table, &input(&recorded)).is_empty());
    }

    #[test]
    fn another_turns_marker_is_not_ours() {
        let mut table = base();
        // The reader reports `Absent` for a marker other than the one asked for.
        table.push(p(300, 1, 300, 30));
        let none = HashSet::new();
        assert!(owned_pids(&table, &input(&none)).is_empty());
        assert!(!env_has_marker(
            [b"CHOCOFACTORY_TURN_bb=1".as_slice()].into_iter(),
            "CHOCOFACTORY_TURN_aa"
        ));
        assert!(env_has_marker(
            [b"X=1".as_slice(), b"CHOCOFACTORY_TURN_aa=1".as_slice()].into_iter(),
            "CHOCOFACTORY_TURN_aa"
        ));
        // A longer name sharing the prefix is not a match either.
        assert!(!env_has_marker(
            [b"CHOCOFACTORY_TURN_aab=1".as_slice()].into_iter(),
            "CHOCOFACTORY_TURN_aa"
        ));
    }

    #[test]
    fn a_child_that_started_before_its_parent_is_not_a_descendant() {
        let mut table = base();
        table.push(p(300, A, 300, 30));
        // Claims 300 as parent but started earlier: 300's pid was reused.
        table.push(p(301, 300, 301, 25));
        assert_eq!(descendants(&table, A), vec![300]);
        let none = HashSet::new();
        assert_eq!(owned_pids(&table, &input(&none)), vec![300]);
    }

    #[test]
    fn the_real_table_lists_this_process_and_its_parent() {
        let table = read(None).expect("the process table is readable");
        let me = std::process::id() as i32;
        let mine = table.iter().find(|e| e.pid == me).expect("this process");
        assert!(!mine.zombie);
        assert_eq!(mine.marker, MarkerStatus::NotChecked);
        assert!(table.iter().any(|e| e.pid == mine.ppid));
    }

    #[test]
    fn the_real_table_finds_a_marker_in_a_readable_environment() {
        // Needs a child whose environment the OS lets us read: a shell
        // builtin-only script is Apple-hidden on macOS, so use this test
        // binary's own `env`-free child: the current executable.
        let name = "CHOCOFACTORY_TURN_00000000000000000000000000unit";
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args(["--exact", "proc_table::tests::never_matches", "--nocapture"])
            .env(name, "1")
            .env("PROC_TABLE_HOLD", "1")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let mut status = MarkerStatus::Absent;
        for _ in 0..100 {
            let table = read(Some(name)).unwrap();
            if let Some(e) = table.iter().find(|e| e.pid == pid) {
                status = e.marker;
                if status == MarkerStatus::Present {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(status, MarkerStatus::Present);
    }

    #[test]
    fn a_pid_list_that_fills_the_buffer_is_retried_larger() {
        let all: Vec<i32> = (1..=100).collect();
        let mut calls = 0;
        let got = fill_pids(4, |buf| {
            calls += 1;
            let n = buf.len().min(all.len());
            buf[..n].copy_from_slice(&all[..n]);
            Ok(n)
        })
        .unwrap();
        assert_eq!(got, all);
        assert!(calls > 1);
        // An error from the lister is passed on, never turned into a list.
        assert!(fill_pids(4, |_| Err(io::Error::other("no"))).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_small_starting_buffer_still_reads_every_process() {
        let name = "CHOCOFACTORY_TURN_0000000000000000000000000small";
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args(["--exact", "proc_table::tests::never_matches", "--nocapture"])
            .env(name, "1")
            .env("PROC_TABLE_HOLD", "1")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let me = std::process::id() as i32;
        let mut found = None;
        for _ in 0..100 {
            // Capacity 4 is far below the live count, and this child is
            // older than whatever else the machine starts meanwhile.
            let table = platform::read_with_capacity(Some(name), 4).unwrap();
            assert!(table.iter().any(|e| e.pid == me), "this process is listed");
            found = table.into_iter().find(|e| e.pid == pid);
            if found
                .as_ref()
                .is_some_and(|e| e.marker == MarkerStatus::Present)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let entry = found.expect("the child is listed");
        assert_eq!(entry.marker, MarkerStatus::Present);
        let none = HashSet::new();
        let input = OwnershipInput {
            agent: None,
            recorded: &none,
            daemon_pid: me,
            daemon_sid: -5,
            uid: entry.uid,
        };
        assert!(owned_pids(&[entry.clone()], &input).contains(&pid));
    }

    #[test]
    fn a_seed_in_the_daemons_session_owns_nothing_else_there_without_the_agent() {
        // The agent is absent from the table (an exited, unreaped agent).
        let mut table = vec![p(D, 1, DS, 10)];
        let mut seed = p(300, 1, DS, 30);
        seed.marker = MarkerStatus::Present;
        table.push(seed);
        table.push(p(301, 1, DS, 31));
        let none = HashSet::new();
        assert_eq!(owned_pids(&table, &input(&none)), vec![300]);
    }

    #[test]
    fn an_unreadable_or_unchecked_environment_is_not_a_marker() {
        for status in [MarkerStatus::Unreadable, MarkerStatus::NotChecked] {
            let mut table = base();
            let mut orphan = p(300, 1, 300, 30);
            orphan.marker = status;
            table.push(orphan);
            table.push(p(301, 1, 300, 31));
            let none = HashSet::new();
            assert!(owned_pids(&table, &input(&none)).is_empty(), "{status:?}");
        }
    }

    #[test]
    fn never_matches() {
        // Run as a child by the test above: stays alive briefly so its
        // environment can be read.
        if std::env::var_os("PROC_TABLE_HOLD").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    #[test]
    fn linux_stat_is_parsed_after_the_last_paren() {
        let stat = "123 (we ) ird (name) S 7 8 9 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 4242 0";
        let (state, ppid, pgrp, session, start, comm) = parse_linux_stat(stat).unwrap();
        assert_eq!((state, ppid, pgrp, session, start), ('S', 7, 8, 9, 4242));
        assert_eq!(comm, "we ) ird (name");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn procargs2_environment_is_split_from_arguments() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend(b"/bin/x\0\0\0x\0arg\0A=1\0B=2\0");
        let env = platform::parse_procargs2(&buf).unwrap();
        assert_eq!(env, vec![b"A=1".to_vec(), b"B=2".to_vec()]);
    }

    #[test]
    fn killing_a_pid_that_is_gone_is_not_an_error_and_not_a_kill() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(matches!(kill_pid(pid), PidKill::Gone));
        assert!(matches!(kill_pid(0), PidKill::Gone));
        assert!(matches!(kill_pid(1), PidKill::Gone));
    }
}
