//! CPU and memory of every session's process tree, read from `/proc`; on
//! Windows from a Toolhelp snapshot and per-process queries.

#[cfg(unix)]
use std::fs;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use crate::{
    collector::{Local, LocalCmd},
    model::ProcInfo,
    state::ProcPoll,
    util::{now_ms, truncate},
};

/// `USER_HZ`, the unit of CPU times in `/proc/<pid>/stat`; Windows times
/// are converted to it.
const TICKS_PER_SEC: f64 = 100.0;
const TOP: usize = 5;
const CMD_LEN: usize = 160;
/// PSS costs ~1.5 ms per large process to read, so it is refreshed less often.
const MEM_EVERY_MS: i64 = 30_000;
const SECRET_WORDS: &[&str] = &["token", "secret", "passw", "apikey", "api-key", "api_key"];
/// In the command line of the shell Claude Code runs a Bash call in.
const SHELL_MARK: &str = "/shell-snapshots/";
/// A shell may have started this long before its call's PreToolUse was
/// stamped: start times come in ticks, and the stamps from another process.
const SHELL_SLACK_MS: i64 = 2_000;
/// How long a command gets to honour SIGTERM.
#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(3);

struct Stat {
    ppid: u32,
    /// The process group; on Windows the process itself.
    pgrp: u32,
    comm: String,
    /// utime + stime + cutime + cstime. A reaped child's total moves into its
    /// parent's cutime, so the sum over a tree only grows. On Windows the
    /// process's own times only.
    ticks: u64,
    /// Start time in ticks since boot; on Windows since the Unix epoch.
    start: u64,
}

/// CPU totals from the previous poll, keyed by `(pid, start)` so a reused pid
/// starts fresh.
pub struct Poller {
    procs: HashMap<(u32, u64), u64>,
    /// Memory per process and when it was read.
    mem: HashMap<(u32, u64), (u64, i64)>,
    /// Parent session of forked session roots, read once per process.
    forks: HashMap<(u32, u64), Option<String>>,
    trees: HashMap<(u32, u64), u64>,
    last: Option<i64>,
    boot_ms: i64,
}

impl Poller {
    pub fn new() -> Self {
        Self {
            procs: HashMap::new(),
            mem: HashMap::new(),
            forks: HashMap::new(),
            trees: HashMap::new(),
            last: None,
            boot_ms: boot_ms(),
        }
    }

    /// Measures the trees under `roots` (`(session id, pid)`). CPU figures are
    /// averages since the previous call and read 0 on the first one.
    pub fn poll(&mut self, roots: &[(String, u32)], now: i64) -> Vec<ProcPoll> {
        let pids: Vec<u32> = roots.iter().map(|(_, pid)| *pid).collect();
        let table = scan(&pids);
        let children = children_of(&table);
        let elapsed = self
            .last
            .map(|last| (now - last) as f64 / 1000.0)
            .filter(|secs| *secs > 0.0);
        let pct =
            |ticks: u64| elapsed.map_or(0.0, |secs| ticks as f64 / TICKS_PER_SEC / secs * 100.0);

        let (mut procs, mut trees, mut out) = (HashMap::new(), HashMap::new(), Vec::new());
        let mut mem = HashMap::new();
        let mut forks = HashMap::new();
        for (session, root) in roots {
            let Some(root_stat) = table.get(root) else {
                continue;
            };
            let members = subtree(*root, &children);
            let total: u64 = members.iter().map(|pid| table[pid].ticks).sum();
            let tree_key = (*root, root_stat.start);
            let prev_total = self.trees.get(&tree_key).copied();
            trees.insert(tree_key, total);
            let forked_from = self
                .forks
                .remove(&tree_key)
                .unwrap_or_else(|| fork_parent(&args(*root)));
            forks.insert(tree_key, forked_from.clone());

            let mut mem_total = 0;
            let mut members_ticks = 0;
            let mut infos: Vec<ProcInfo> = members
                .iter()
                .map(|pid| {
                    let stat = &table[pid];
                    let key = (*pid, stat.start);
                    let started_at =
                        self.boot_ms + (stat.start as f64 * 1000.0 / TICKS_PER_SEC) as i64;
                    let ticks = match self.procs.get(&key) {
                        Some(prev) => stat.ticks.saturating_sub(*prev),
                        None if self.last.is_some_and(|last| started_at >= last) => stat.ticks,
                        None => 0,
                    };
                    members_ticks += ticks;
                    procs.insert(key, stat.ticks);
                    let mem_bytes = match self.mem.get(&key) {
                        Some(&(bytes, at)) if now - at < MEM_EVERY_MS => {
                            mem.insert(key, (bytes, at));
                            bytes
                        }
                        _ => {
                            let bytes = mem_bytes(*pid);
                            mem.insert(key, (bytes, now));
                            bytes
                        }
                    };
                    mem_total += mem_bytes;
                    ProcInfo {
                        pid: *pid,
                        name: stat.comm.clone(),
                        cmd: String::new(),
                        cpu_pct: pct(ticks),
                        mem_bytes,
                        started_at,
                    }
                })
                .collect();
            // NOTE: Windows keeps no reaped children's times: a tree's total drops when one
            // exits, so its members' own deltas count instead
            let cpu_pct = prev_total.map_or(0.0, |prev| {
                pct(if cfg!(unix) {
                    total.saturating_sub(prev)
                } else {
                    members_ticks
                })
            });
            infos.sort_by(|a, b| {
                b.cpu_pct
                    .total_cmp(&a.cpu_pct)
                    .then(b.mem_bytes.cmp(&a.mem_bytes))
            });
            infos.truncate(TOP);
            for info in &mut infos {
                let args = args(info.pid);
                info.name = display_name(&info.name, &args);
                info.cmd = truncate(&mask_secrets(&args).join(" "), CMD_LEN);
            }
            out.push(ProcPoll {
                session: session.clone(),
                cpu_pct,
                mem_bytes: mem_total,
                procs: u32::try_from(members.len()).unwrap_or(u32::MAX),
                top: infos,
                forked_from,
            });
        }
        self.procs = procs;
        self.mem = mem;
        self.forks = forks;
        self.trees = trees;
        self.last = Some(now);
        out
    }
}

pub async fn run(local: Local, every: Duration) {
    let tx = local.sender();
    let mut poller = Poller::new();
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Ok(roots) = local.pids().await else {
            return;
        };
        let now = now_ms();
        let Ok((back, polls)) = tokio::task::spawn_blocking(move || {
            let polls = poller.poll(&roots, now);
            (poller, polls)
        })
        .await
        else {
            return;
        };
        poller = back;
        if tx.send(LocalCmd::Procs(polls, now)).await.is_err() {
            return;
        }
    }
}

#[cfg(unix)]
fn boot_ms() -> i64 {
    fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|stat| {
            stat.lines()
                .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())
        })
        .map_or(0, |secs| secs * 1000)
}

/// Windows start times count from the Unix epoch.
#[cfg(windows)]
fn boot_ms() -> i64 {
    0
}

/// A child of a Claude process, as `running_shells` judges it.
struct Child {
    pid: u32,
    pgrp: u32,
    /// Ticks since boot: with the pid, who this is.
    start: u64,
    started_ms: i64,
    args: String,
}

/// Which children are the shells of the Bash calls running since `since_ms`.
/// Claude Code starts each call's shell in a process group of its own, and
/// the shell sources its snapshot first; its MCP servers stay in Claude's
/// group, and a background shell from earlier is older than the running call.
fn running_shells(children: &[Child], since_ms: i64) -> Vec<(u32, u64)> {
    children
        .iter()
        .filter(|c| c.pid == c.pgrp && c.args.contains(SHELL_MARK))
        .filter(|c| c.started_ms >= since_ms - SHELL_SLACK_MS)
        .map(|c| (c.pid, c.start))
        .collect()
}

/// Asks the group `pgrp` to end, by force with `force`.
#[cfg(unix)]
pub fn end_group(pgrp: u32, force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: `kill` takes plain integers; a gone group is an error we ignore
    unsafe { libc::kill(-(pgrp as i32), signal) };
}

/// Ends `pgrp` (a process; Windows has no groups) and everything below
/// it, at once: Windows has no request to end.
#[cfg(windows)]
pub fn end_group(pgrp: u32, _force: bool) {
    end_tree(pgrp, None::<fn()>);
}

/// How long `end_tree` waits for its root to be gone before `root_gone`.
#[cfg(windows)]
const ROOT_GONE: Duration = Duration::from_secs(2);

/// Ends `root` and everything below it, at once, as `end_group`; with
/// `root_gone`, waits for `root` itself to be gone and runs it then, before
/// the rest goes (not at all when `root` would not end).
#[cfg(windows)]
pub fn end_tree(root: u32, mut root_gone: Option<impl FnOnce()>) {
    let table = scan(&[root]);
    // NOTE: gone already, or not to be queried
    if !table.contains_key(&root) {
        return;
    }
    // NOTE: parents first, so none is left to start a replacement for an ended child
    let mut ended: Vec<(u32, u64)> = Vec::new();
    for pid in subtree(root, &children_of(&table)) {
        let terminated = crate::win::terminate(pid).is_ok();
        ended.extend(table.get(&pid).map(|stat| (pid, stat.start)));
        // NOTE: the root comes first
        if pid == root
            && terminated
            && root_gone.is_some()
            && crate::win::wait_gone(root, ROOT_GONE)
            && let Some(then) = root_gone.take()
        {
            then();
        }
    }
    // NOTE: and what one started between the snapshot and its end
    for _ in 0..3 {
        let late: Vec<(u32, u64)> = crate::win::processes()
            .into_iter()
            .filter(|p| ended.iter().any(|(pid, _)| *pid == p.ppid))
            .filter_map(|p| {
                let start = start_of(p.pid)?;
                let parent = ended.iter().find(|(pid, _)| *pid == p.ppid)?;
                (start >= parent.1 && !ended.iter().any(|(pid, _)| *pid == p.pid))
                    .then_some((p.pid, start))
            })
            .collect();
        if late.is_empty() {
            break;
        }
        for &(pid, _) in &late {
            let _ = crate::win::terminate(pid);
        }
        ended.extend(late);
    }
}

/// Ends the Bash calls the Claude process `claude` is waiting on, as Esc in
/// the terminal does: the shell's whole group, so the command goes too, and
/// by force if it ignores the request. A background shell stays. How many
/// were asked to stop.
pub async fn end_tool_shells(claude: u32, since_ms: i64) -> usize {
    // NOTE: off the runtime's thread: reading the table and ending trees blocks
    // (on Windows a snapshot and a query per process)
    let shells = tokio::task::spawn_blocking(move || {
        let shells = tool_shells(claude, since_ms);
        for &(pid, _) in &shells {
            end_group(pid, false);
        }
        shells
    })
    .await
    .unwrap_or_default();
    let count = shells.len();
    // NOTE: Windows ended the trees at once; SIGTERM may be ignored
    #[cfg(unix)]
    tokio::spawn(async move {
        tokio::time::sleep(TERM_GRACE).await;
        for (pid, start) in shells {
            // NOTE: the start time tells a stubborn shell from a reused pid
            if start_of(pid) == Some(start) {
                end_group(pid, true);
            }
        }
    });
    count
}

/// The shells of the Bash calls `claude` runs since `since_ms`, with their
/// start times.
fn tool_shells(claude: u32, since_ms: i64) -> Vec<(u32, u64)> {
    let boot = boot_ms();
    let table = scan(&[claude]);
    let Some(claude_start) = table.get(&claude).map(|stat| stat.start) else {
        return Vec::new();
    };
    let children: Vec<Child> = table
        .into_iter()
        .filter(|(_, stat)| stat.ppid == claude && stat.start >= claude_start)
        .map(|(pid, stat)| Child {
            pid,
            pgrp: stat.pgrp,
            start: stat.start,
            started_ms: boot + (stat.start as f64 / TICKS_PER_SEC * 1000.0) as i64,
            args: args(pid).join(" "),
        })
        .collect();
    running_shells(&children, since_ms)
}

/// Every process: `/proc` holds the parents only per process.
#[cfg(unix)]
fn scan(_roots: &[u32]) -> HashMap<u32, Stat> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    dir.filter_map(|entry| {
        let pid = entry.ok()?.file_name().to_str()?.parse::<u32>().ok()?;
        Some((pid, read_stat(pid)?))
    })
    .collect()
}

#[cfg(unix)]
fn read_stat(pid: u32) -> Option<Stat> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// When `pid` started, in [`Stat::start`]'s units; none when it is gone.
#[cfg(unix)]
fn start_of(pid: u32) -> Option<u64> {
    read_stat(pid).map(|stat| stat.start)
}

/// `roots` and the processes under them, from the snapshot's parent ids,
/// querying only those: a child must have started no earlier than its
/// parent. Times in [`TICKS_PER_SEC`] units.
#[cfg(windows)]
fn scan(roots: &[u32]) -> HashMap<u32, Stat> {
    let procs = crate::win::processes();
    let mut by_parent: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut by_pid: HashMap<u32, usize> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        by_parent.entry(p.ppid).or_default().push(i);
        by_pid.insert(p.pid, i);
    }
    let stat_of = |i: usize| {
        let p = &procs[i];
        let times = crate::win::process_times(p.pid)?;
        Some(Stat {
            ppid: p.ppid,
            pgrp: p.pid,
            comm: p.exe.clone(),
            ticks: times.cpu_100ns / 100_000,
            start: start_ticks(&times)?,
        })
    };
    let mut table = HashMap::new();
    let mut queue = Vec::new();
    for &root in roots {
        if let Some(stat) = by_pid.get(&root).and_then(|&i| stat_of(i)) {
            table.insert(root, stat);
            queue.push(root);
        }
    }
    while let Some(pid) = queue.pop() {
        let start = table[&pid].start;
        for &i in by_parent.get(&pid).into_iter().flatten() {
            let kid = procs[i].pid;
            if kid == pid || table.contains_key(&kid) {
                continue;
            }
            if let Some(stat) = stat_of(i).filter(|stat| stat.start >= start) {
                table.insert(kid, stat);
                queue.push(kid);
            }
        }
    }
    table
}

#[cfg(windows)]
fn start_ticks(times: &crate::win::Times) -> Option<u64> {
    u64::try_from(times.started_ms / 10).ok()
}

/// When `pid` started, in [`Stat::start`]'s units; none when it is gone.
#[cfg(windows)]
fn start_of(pid: u32) -> Option<u64> {
    start_ticks(&crate::win::process_times(pid)?)
}

#[cfg(unix)]
fn parse_stat(raw: &str) -> Option<Stat> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    // NOTE: comm may contain spaces and parens; fields restart after the last ')'
    let fields: Vec<&str> = raw.get(close + 2..)?.split(' ').collect();
    let field = |n: usize| fields.get(n - 3)?.parse::<i64>().ok();
    let ticks = [14, 15, 16, 17]
        .iter()
        .map(|&n| field(n).map(|v| v.max(0) as u64))
        .sum::<Option<u64>>()?;
    Some(Stat {
        ppid: u32::try_from(field(4)?).ok()?,
        pgrp: u32::try_from(field(5)?).ok()?,
        comm: raw[open + 1..close].to_owned(),
        ticks,
        start: u64::try_from(field(22)?).ok()?,
    })
}

/// Parent → children, through parents that started no later than the
/// child: Windows never reparents an orphan, so its parent's pid may be
/// another process's by now.
fn children_of(table: &HashMap<u32, Stat>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, stat) in table {
        if table
            .get(&stat.ppid)
            .is_some_and(|parent| parent.start <= stat.start)
        {
            children.entry(stat.ppid).or_default().push(*pid);
        }
    }
    children
}

fn subtree(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut members = vec![root];
    let mut seen = HashSet::from([root]);
    let mut i = 0;
    while i < members.len() {
        if let Some(kids) = children.get(&members[i]) {
            for kid in kids {
                if seen.insert(*kid) {
                    members.push(*kid);
                }
            }
        }
        i += 1;
    }
    members
}

/// PSS from `smaps_rollup`, falling back to RSS where that is unreadable.
#[cfg(unix)]
fn mem_bytes(pid: u32) -> u64 {
    let field = |file: &str, key: &str| {
        let text = fs::read_to_string(format!("/proc/{pid}/{file}")).ok()?;
        let line = text.lines().find(|l| l.starts_with(key))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok()
    };
    field("smaps_rollup", "Pss:")
        .or_else(|| field("status", "VmRSS:"))
        .map_or(0, |kib| kib * 1024)
}

/// The working set.
#[cfg(windows)]
fn mem_bytes(pid: u32) -> u64 {
    crate::win::process_memory(pid).unwrap_or(0)
}

/// The command line of `pid`; none when it is gone.
#[cfg(unix)]
pub fn cmdline(pid: u32) -> Option<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
}

/// The command line of `pid`; none when it is gone or not this user's.
#[cfg(windows)]
pub fn cmdline(pid: u32) -> Option<String> {
    crate::win::command_line(pid)
}

#[cfg(windows)]
fn args(pid: u32) -> Vec<String> {
    cmdline(pid)
        .map(|line| crate::win::split_command_line(&line))
        .unwrap_or_default()
}

/// The direct children of `pid`, with their names.
#[cfg(windows)]
pub fn children(pid: u32) -> Vec<(u32, String)> {
    let Some(start) = start_of(pid) else {
        return Vec::new();
    };
    crate::win::processes()
        .into_iter()
        .filter(|p| p.ppid == pid && p.pid != pid && start_of(p.pid).is_some_and(|s| s >= start))
        .map(|p| (p.pid, p.exe))
        .collect()
}

/// The direct children of `pid`, with their names.
#[cfg(unix)]
pub fn children(pid: u32) -> Vec<(u32, String)> {
    let table = scan(&[pid]);
    let Some(start) = table.get(&pid).map(|stat| stat.start) else {
        return Vec::new();
    };
    table
        .into_iter()
        .filter(|(child, stat)| stat.ppid == pid && *child != pid && stat.start >= start)
        .map(|(child, stat)| (child, stat.comm))
        .collect()
}

/// The process that started this one: on Windows the nearest one above it
/// that is not a shell, since hooks and MCP servers may run under one
/// there, and none when no parent can be told.
#[cfg(unix)]
pub fn parent_id() -> Option<u32> {
    Some(std::os::unix::process::parent_id())
}

/// What may stand between Claude Code (`claude.exe`, or `node.exe` from
/// npm) and a hook on Windows.
#[cfg(windows)]
const SHELLS: &[&str] = &[
    "bash.exe",
    "sh.exe",
    "dash.exe",
    "zsh.exe",
    "env.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
];

/// Asks each ancestor on its own: a hook runs this every time.
#[cfg(windows)]
pub fn parent_id() -> Option<u32> {
    first_above_shells(
        std::process::id(),
        crate::win::parent_of,
        crate::win::exe_name,
        start_of,
    )
}

/// The nearest process above `pid` that is not a shell, else the highest
/// shell; none when even the parent cannot be told (gone, its pid reused,
/// not to be opened).
#[cfg(windows)]
fn first_above_shells(
    mut pid: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
    exe_of: impl Fn(u32) -> Option<String>,
    start_of: impl Fn(u32) -> Option<u64>,
) -> Option<u32> {
    let mut start = start_of(pid);
    let mut found = None;
    for _ in 0..8 {
        let Some(ppid) = parent_of(pid) else {
            break;
        };
        // NOTE: Windows never reparents: a later start is another process with the gone parent's pid
        let Some(ppid_start) = start_of(ppid).filter(|s| start.is_some_and(|child| *s <= child))
        else {
            break;
        };
        found = Some(ppid);
        if !exe_of(ppid).is_some_and(|exe| SHELLS.contains(&exe.to_ascii_lowercase().as_str())) {
            break;
        }
        (pid, start) = (ppid, Some(ppid_start));
    }
    found
}

#[cfg(unix)]
fn args(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

/// The session a `claude --fork-session --resume <id or transcript>` process
/// continues from.
fn fork_parent(args: &[String]) -> Option<String> {
    if !args.iter().any(|a| a == "--fork-session") {
        return None;
    }
    let i = args.iter().position(|a| a == "--resume" || a == "-r")?;
    let value = args.get(i + 1)?;
    let id = value
        .rsplit(std::path::is_separator)
        .next()?
        .trim_end_matches(".jsonl");
    let uuid = id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    uuid.then(|| id.to_owned())
}

/// A readable program name: the Nix package of the program (or of the
/// script an interpreter runs), else the executable without wrapper noise.
fn display_name(comm: &str, args: &[String]) -> String {
    let base = |arg: &str| {
        let name = arg.rsplit(std::path::is_separator).next().unwrap_or(arg);
        // NOTE: Windows names executables in any case (NODE.EXE)
        let exe = name.len().checked_sub(4).filter(|&at| {
            cfg!(windows)
                && name
                    .get(at..)
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(".exe"))
        });
        exe.map_or(name, |at| &name[..at]).to_owned()
    };
    let interpreter = args.first().is_some_and(|arg| {
        let name = base(arg).to_ascii_lowercase();
        [
            "node", "python", "bash", "sh", "bun", "deno", "ruby", "perl",
        ]
        .iter()
        .any(|i| name.starts_with(i))
    });
    let program = if interpreter {
        args.get(1)
    } else {
        args.first()
    };
    if let Some(name) = program.and_then(|arg| nix_package(arg)) {
        return name;
    }
    let name = program.map_or_else(|| base(comm), |arg| base(arg));
    let name = name.trim_start_matches('.');
    name.strip_suffix("-wrapped").unwrap_or(name).to_owned()
}

/// `/nix/store/<hash>-playwright-mcp-0.0.82/lib/…` → `playwright-mcp`.
fn nix_package(path: &str) -> Option<String> {
    let entry = path.strip_prefix("/nix/store/")?.split('/').next()?;
    let (_, name) = entry.split_once('-')?;
    let parts: Vec<&str> = name
        .split('-')
        .take_while(|part| !part.starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("-"))
}

/// Hides values of `--token=x`, `--api-key x` and similar arguments.
fn mask_secrets(args: &[String]) -> Vec<String> {
    let secret = |arg: &str| {
        let lower = arg.to_ascii_lowercase();
        SECRET_WORDS.iter().any(|word| lower.contains(word))
    };
    let mut out = Vec::with_capacity(args.len());
    let mut mask_next = false;
    for arg in args {
        if mask_next {
            out.push("…".to_owned());
            mask_next = false;
        } else if let Some((key, _)) = arg.split_once('=').filter(|(key, _)| secret(key)) {
            out.push(format!("{key}=…"));
        } else {
            mask_next = arg.starts_with('-') && secret(arg);
            out.push(arg.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parses_stat_with_odd_comm() {
        let raw = concat!(
            "4242 (tmux: server (1)) S 1 4242 4242 0 -1 4194560 100 0 0 0 ",
            "250 50 7 3 20 0 1 0 1234567 10000 300 18446744073709551615",
        );
        let stat = parse_stat(raw).unwrap();
        assert_eq!(stat.comm, "tmux: server (1)");
        assert_eq!((stat.ppid, stat.ticks, stat.start), (1, 310, 1_234_567));
        assert_eq!(stat.pgrp, 4242);
    }

    #[test]
    fn only_the_running_call_s_shell_is_ended() {
        const SHELL: &str = "bash -c source ~/.claude/shell-snapshots/snapshot-bash-1.sh && eval x";
        let child = |pid, pgrp, started_ms, args: &str| Child {
            pid,
            pgrp,
            start: u64::from(pid),
            started_ms,
            args: args.to_owned(),
        };
        let since = 100_000;
        let picked = running_shells(
            &[
                child(10, 10, since + 50, SHELL),
                child(11, 11, since - 500, SHELL),
                child(20, 5, since + 50, "ccdash channel"),
                child(21, 21, since + 50, "node mcp-server.js"),
                child(22, 5, since + 50, SHELL),
                child(30, 30, since - 60_000, SHELL),
            ],
            since,
        );
        assert_eq!(
            picked,
            [(10, 10), (11, 11)],
            "not an MCP server, a shell in Claude's group, or an older background shell"
        );
    }

    #[test]
    fn subtree_walks_all_descendants() {
        let children = HashMap::from([(1, vec![2, 3]), (2, vec![4]), (9, vec![10])]);
        let mut members = subtree(1, &children);
        members.sort_unstable();
        assert_eq!(members, [1, 2, 3, 4]);
        let cycle = HashMap::from([(0, vec![0, 1]), (1, vec![2]), (2, vec![1])]);
        let mut members = subtree(0, &cycle);
        members.sort_unstable();
        assert_eq!(members, [0, 1, 2]);
    }

    #[test]
    fn a_parent_that_started_later_is_not_one() {
        let stat = |ppid, start| Stat {
            ppid,
            pgrp: 0,
            comm: String::new(),
            ticks: 0,
            start,
        };
        // NOTE: 7's parent is gone; 5 took its pid later
        let table = HashMap::from([(5, stat(1, 100)), (6, stat(5, 120)), (7, stat(5, 50))]);
        let children = children_of(&table);
        assert_eq!(children.get(&5), Some(&vec![6]));
        assert_eq!(subtree(5, &children), [5, 6]);
    }

    #[test]
    fn masks_secret_arguments() {
        let args: Vec<String> = ["mcp", "--api-key", "abc", "--token=xyz", "--port", "80"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            mask_secrets(&args).join(" "),
            "mcp --api-key … --token=… --port 80"
        );
    }

    #[test]
    fn readable_names() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            display_name(
                "MainThread",
                &args(&[
                    "/nix/store/v6w-nodejs-24.20.0/bin/node",
                    "/nix/store/wg3-playwright-mcp-0.0.82/lib/node_modules/cli.js"
                ])
            ),
            "playwright-mcp"
        );
        assert_eq!(
            display_name(
                ".claude-wrapped",
                &args(&["/nix/store/ayk-claude-code-2.1.296/bin/.claude-wrapped"])
            ),
            "claude-code"
        );
        assert_eq!(display_name("cargo", &args(&["cargo", "build"])), "cargo");
        assert_eq!(display_name(".foo-wrapped", &[]), "foo");
    }

    #[test]
    fn finds_the_forked_session() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            fork_parent(&args(&[
                ".claude-wrapped",
                "--session-id",
                "5175e184-971a-4662-b207-6bf37c73bd66",
                "--fork-session",
                "--resume",
                "/home/u/.claude/projects/x/bd459acf-f32e-4e9b-9a8e-ef2ee7758fca.jsonl",
            ])),
            Some("bd459acf-f32e-4e9b-9a8e-ef2ee7758fca".into())
        );
        assert_eq!(fork_parent(&args(&["claude", "--resume", "abc"])), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_give_names_and_forks() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            display_name("claude.exe", &args(&[r"C:\Users\u\.local\bin\claude.exe"])),
            "claude"
        );
        assert_eq!(
            display_name(
                "NODE.EXE",
                &args(&[r"C:\NODEJS\NODE.EXE", r"C:\app\cli.js"])
            ),
            "cli.js"
        );
        assert_eq!(display_name("claude.exe", &[]), "claude");
        assert_eq!(
            fork_parent(&args(&[
                "claude",
                "--fork-session",
                "--resume",
                r"C:\Users\u\.claude\projects\x\bd459acf-f32e-4e9b-9a8e-ef2ee7758fca.jsonl",
            ])),
            Some("bd459acf-f32e-4e9b-9a8e-ef2ee7758fca".into())
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_session_above_its_hook_s_shells() {
        let chain: HashMap<u32, (u32, &str)> = HashMap::from([
            (10, (9, "ccdash.exe")),
            (9, (8, "bash.exe")),
            (8, (7, "node.exe")),
            (7, (1, "WindowsTerminal.exe")),
        ]);
        let parent = |pid: u32| chain.get(&pid).map(|(ppid, _)| *ppid);
        let exe = |pid: u32| chain.get(&pid).map(|(_, exe)| (*exe).to_owned());
        let in_order = |pid: u32| Some(u64::from(pid));
        assert_eq!(
            first_above_shells(10, parent, exe, in_order),
            Some(8),
            "npm's node.exe"
        );
        // NOTE: 8 started after its child: the pid was reused
        let reused = |pid: u32| Some(if pid == 8 { 500 } else { u64::from(pid) });
        assert_eq!(first_above_shells(10, parent, exe, reused), Some(9));
        assert_eq!(first_above_shells(10, parent, exe, |_| None), None);
    }

    #[cfg(windows)]
    #[test]
    fn ends_a_child_s_tree() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let me = std::process::id();
        assert!(children(me).iter().any(|(pid, _)| *pid == child.id()));
        let mut grandchild = None;
        for _ in 0..50 {
            grandchild = children(child.id()).first().map(|(pid, _)| *pid);
            if grandchild.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let grandchild = grandchild.expect("ping under cmd");
        assert!(scan(&[me]).contains_key(&grandchild));
        let tree = scan(&[child.id()]);
        assert!(tree.contains_key(&grandchild) && !tree.contains_key(&me));
        end_group(child.id(), true);
        assert!(!child.wait().unwrap().success());
        // NOTE: a group gone by the time it is ended
        end_group(child.id(), true);
        // NOTE: TerminateProcess returns before the process is gone
        let gone = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            !crate::win::pid_alive(grandchild)
        });
        assert!(gone, "{grandchild} still runs");
    }

    /// The console is restored between the root's end and its children's:
    /// the shell prompts again as soon as Claude Code is gone.
    #[cfg(windows)]
    #[test]
    fn a_tree_s_root_is_gone_before_its_children() {
        use std::os::windows::process::CommandExt;
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let root = child.id();
        let grandchild = (0..50)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                children(root).first().map(|(pid, _)| *pid)
            })
            .expect("ping under cmd");
        let mut seen = None;
        end_tree(
            root,
            Some(|| {
                seen = Some((
                    crate::win::pid_alive(root),
                    crate::win::pid_alive(grandchild),
                ));
            }),
        );
        assert_eq!(seen, Some((false, true)), "(root, child) running then");
        assert!(!child.wait().unwrap().success());
        assert!(crate::win::wait_gone(grandchild, Duration::from_secs(2)));
        let mut again = false;
        end_tree(root, Some(|| again = true));
        assert!(!again, "nothing to end, nothing to run");
    }

    #[test]
    fn measures_this_process() {
        let mut poller = Poller::new();
        let roots = [("s".to_owned(), std::process::id())];
        let first = poller.poll(&roots, now_ms());
        assert_eq!(first.len(), 1);
        assert!(first[0].mem_bytes > 0);
        assert!(first[0].top.iter().any(|p| p.pid == std::process::id()));
    }
}
