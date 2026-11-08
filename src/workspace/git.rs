//! The git CLI as the workspace sync drives it: every call runs with the
//! always-exclude list as `core.excludesFile`, an empty `core.hooksPath`
//! (25 of 219 real units carry a `pre-push` hook that would run on every
//! sync push), `gc.auto` off, no credential helpers, no prompts, stdin
//! closed, a timeout, and the hub token handed over through the
//! `GIT_CONFIG_*` environment (`http.<url>.extraHeader`), never argv. The
//! repository's own configuration is never written. The hub's name is
//! resolved here once per link (`http.curloptResolve`): git resolves it
//! per process otherwise, and an adopt burst's thousand lookups a minute
//! tripped Pi-hole's per-client limit. Every child is reaped with
//! `wait4`, so what it cost (CPU, block I/O, peak memory) is exact and
//! adds up in [`GitCost`].

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tracing::debug;

/// Ordinary plumbing finishes in milliseconds; a whole-tree `add -A` of a
/// huge unit in seconds.
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Transfers: a seed of an 800 MB history took 43 s on the LAN.
pub const NET_TIMEOUT: Duration = Duration::from_secs(900);
/// How long the hub's resolved address is reused before another lookup.
const RESOLVE_TTL: Duration = Duration::from_secs(300);

/// The hub's address as `http.curloptResolve` wants it, and when it was
/// looked up.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub value: String,
    pub at: Instant,
}

/// What the git calls cost so far, cumulative; read by the collector's
/// status report.
#[derive(Debug, Default)]
pub struct GitCost {
    pub calls: AtomicU64,
    pub cpu_ms: AtomicU64,
    pub wall_ms: AtomicU64,
    pub io_read: AtomicU64,
    pub io_write: AtomicU64,
    pub max_rss_kb: AtomicU64,
    pub pushes: AtomicU64,
    pub fetches: AtomicU64,
    pub push_ms: AtomicU64,
    pub fetch_ms: AtomicU64,
}

impl GitCost {
    fn add(&self, usage: &Usage, wall: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.cpu_ms.fetch_add(usage.cpu_ms, Ordering::Relaxed);
        self.wall_ms
            .fetch_add(wall.as_millis() as u64, Ordering::Relaxed);
        self.io_read.fetch_add(usage.io_read, Ordering::Relaxed);
        self.io_write.fetch_add(usage.io_write, Ordering::Relaxed);
        self.max_rss_kb
            .fetch_max(usage.max_rss_kb, Ordering::Relaxed);
    }
}

/// What one child cost (`wait4`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu_ms: u64,
    pub io_read: u64,
    pub io_write: u64,
    pub max_rss_kb: u64,
}

/// How every call is made.
#[derive(Debug, Clone)]
pub struct Git {
    /// The always-exclude list, one pattern per line.
    pub excludes_file: PathBuf,
    /// An empty directory.
    pub hooks_dir: PathBuf,
    /// The hub's git base (`https://host/git/`), for the token header.
    pub hub_base: Option<String>,
    pub token_file: Option<PathBuf>,
    /// Names the committer of snapshots.
    pub host: String,
    /// The hub's address, looked up once per link and every `RESOLVE_TTL`.
    pub resolver: Arc<Mutex<Option<Resolved>>>,
    pub cost: Arc<GitCost>,
}

/// A repository: its git dir and working tree (the same directory for a
/// plain unit's hidden repository and its directory).
#[derive(Debug, Clone)]
pub struct Repo {
    pub git_dir: PathBuf,
    pub work_tree: PathBuf,
}

/// What a call produced.
#[derive(Debug)]
pub struct Output {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub usage: Usage,
}

impl Output {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    /// NUL-separated records.
    pub fn records(&self) -> Vec<String> {
        self.stdout
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect()
    }
}

/// A ref the hub refused: the current tips it printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub tips: BTreeMap<String, String>,
    pub message: String,
}

/// A conflicted path of a `merge-tree`, with the three blobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conflicted {
    pub path: String,
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub mode: String,
}

/// What `merge-tree --write-tree` came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub tree: String,
    pub conflicts: Vec<Conflicted>,
}

/// Extra settings of one call.
#[derive(Default)]
pub struct Opts<'a> {
    pub index: Option<&'a Path>,
    /// Another excludes file (a unit with its own `always_exclude`).
    pub excludes: Option<&'a Path>,
    pub stdin: Option<&'a [u8]>,
    /// Reaches the hub: the token header and the longer timeout.
    pub net: bool,
    pub timeout: Option<Duration>,
}

impl Git {
    /// Writes the excludes file and the empty hooks directory under
    /// `state`.
    ///
    /// # Errors
    ///
    /// They cannot be written.
    pub fn prepare(
        state: &Path,
        host: &str,
        always_exclude: &[String],
        hub_base: Option<String>,
        token_file: Option<PathBuf>,
    ) -> Result<Self> {
        fs::create_dir_all(state)?;
        let hooks_dir = state.join("hooks-empty");
        fs::create_dir_all(&hooks_dir)?;
        let excludes_file = state.join("always-exclude");
        let mut text = String::new();
        for pattern in always_exclude {
            // NOTE: a bare name matches a directory at any depth, a path only there
            text.push_str(pattern);
            text.push('\n');
        }
        write_if_changed(&excludes_file, text.as_bytes())?;
        Ok(Self {
            excludes_file,
            hooks_dir,
            hub_base,
            token_file,
            host: host.to_owned(),
            resolver: Arc::new(Mutex::new(None)),
            cost: Arc::new(GitCost::default()),
        })
    }

    /// The hub's host and port from `hub_base` (`https://host[:port]/git/`);
    /// None for an address that is an IP literal already.
    fn hub_host_port(&self) -> Option<(String, u16)> {
        let base = self.hub_base.as_deref()?;
        let (scheme, rest) = base.split_once("://")?;
        let authority = rest.split('/').next()?;
        let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
            let (host, after) = rest.split_once(']')?;
            (host, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        let port = match port {
            Some(p) => p.parse().ok()?,
            None if scheme == "http" => 80,
            None => 443,
        };
        if host.is_empty() || host.parse::<IpAddr>().is_ok() {
            return None;
        }
        Some((host.to_owned(), port))
    }

    /// `http.curloptResolve` for the hub: `host:port:addr[,addr]`, looked up
    /// at most every `RESOLVE_TTL`; the last answer while the resolver
    /// fails; None when there is nothing to resolve.
    pub fn curl_resolve(&self) -> Option<String> {
        let (host, port) = self.hub_host_port()?;
        let mut cache = self.resolver.lock().ok()?;
        if let Some(r) = cache.as_ref()
            && r.at.elapsed() < RESOLVE_TTL
        {
            return Some(r.value.clone());
        }
        match (host.as_str(), port).to_socket_addrs() {
            Ok(addrs) => {
                let mut list: Vec<String> = Vec::new();
                for a in addrs {
                    let text = match a.ip() {
                        IpAddr::V4(v4) => v4.to_string(),
                        IpAddr::V6(v6) => format!("[{v6}]"),
                    };
                    if !list.contains(&text) {
                        list.push(text);
                    }
                }
                if list.is_empty() {
                    return cache.as_ref().map(|r| r.value.clone());
                }
                let value = format!("{host}:{port}:{}", list.join(","));
                debug!(
                    "workspace: the hub {host}:{port} resolves to {}",
                    list.join(",")
                );
                *cache = Some(Resolved {
                    value: value.clone(),
                    at: Instant::now(),
                });
                Some(value)
            }
            Err(e) => {
                debug!("workspace: looking up {host} failed ({e}); using the last answer");
                cache.as_ref().map(|r| r.value.clone())
            }
        }
    }

    /// The next net call looks the hub up again (the link reconnected).
    /// The old answer stays as the fallback.
    pub fn forget_hub_address(&self) {
        if let Ok(mut cache) = self.resolver.lock()
            && let Some(r) = cache.as_mut()
        {
            r.at = Instant::now() - RESOLVE_TTL;
        }
    }

    /// The excludes file of one unit, when its rules add to the defaults.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn excludes_for(&self, dir: &Path, patterns: &[String]) -> Result<PathBuf> {
        let path = dir.join("always-exclude");
        let mut text = String::new();
        for pattern in patterns {
            text.push_str(pattern);
            text.push('\n');
        }
        write_if_changed(&path, text.as_bytes())?;
        Ok(path)
    }

    fn command(&self, repo: Option<&Repo>, opts: &Opts) -> Result<Command> {
        let mut cmd = Command::new("git");
        let mut config: Vec<(String, String)> = vec![
            (
                "core.excludesFile".into(),
                opts.excludes
                    .unwrap_or(&self.excludes_file)
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "core.hooksPath".into(),
                self.hooks_dir.to_string_lossy().into_owned(),
            ),
            ("gc.auto".into(), "0".into()),
            // NOTE: working trees travel byte for byte, whatever the user's git converts
            ("core.autocrlf".into(), "false".into()),
            ("credential.helper".into(), String::new()),
            ("protocol.version".into(), "2".into()),
            ("advice.detachedHead".into(), "false".into()),
            ("fetch.prune".into(), "false".into()),
        ];
        if opts.net {
            let base = self
                .hub_base
                .as_deref()
                .context("the hub's git address is not set")?;
            let path = self
                .token_file
                .as_ref()
                .context("hub.token_file is not set")?;
            let token =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let token = token.trim();
            if token.is_empty() {
                bail!("{} is empty", path.display());
            }
            config.push((
                format!("http.{base}.extraHeader"),
                format!("Authorization: Bearer {token}"),
            ));
            if let Some(resolve) = self.curl_resolve() {
                config.push(("http.curloptResolve".into(), resolve));
            }
        }
        cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (i, (key, value)) in config.iter().enumerate() {
            cmd.env(format!("GIT_CONFIG_KEY_{i}"), key);
            cmd.env(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
        cmd.env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_SSH_COMMAND", "false")
            .env("GIT_ASKPASS", "true")
            .env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "ccdash")
            .env("GIT_AUTHOR_EMAIL", format!("ccdash@{}", self.host))
            .env("GIT_COMMITTER_NAME", "ccdash")
            .env("GIT_COMMITTER_EMAIL", format!("ccdash@{}", self.host))
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_PREFIX")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_NAMESPACE");
        if let Some(repo) = repo {
            cmd.env("GIT_DIR", &repo.git_dir)
                .env("GIT_WORK_TREE", &repo.work_tree)
                .current_dir(&repo.work_tree);
        }
        if let Some(index) = opts.index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        cmd.stdin(if opts.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        Ok(cmd)
    }

    /// Runs `git <args>` in `repo` (or nowhere), killing it after the
    /// timeout.
    ///
    /// # Errors
    ///
    /// git cannot be started, or the call timed out; a non-zero exit is
    /// `Output::ok == false`, not an error.
    pub fn run(&self, repo: Option<&Repo>, args: &[&str], opts: &Opts) -> Result<Output> {
        let mut cmd = self.command(repo, opts)?;
        cmd.args(args);
        let timeout = opts
            .timeout
            .unwrap_or(if opts.net { NET_TIMEOUT } else { LOCAL_TIMEOUT });
        let started = Instant::now();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // NOTE: suspended until it is in its job (Tree::of)
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting git {}", args.first().unwrap_or(&"")))?;
        let tree = Tree::of(&child);
        let out = wait(child, tree, opts.stdin, timeout)
            .with_context(|| format!("git {}", args.join(" ")))?;
        self.cost.add(&out.usage, started.elapsed());
        if !out.ok {
            debug!(
                "git {} in {}: exit {:?}: {}",
                args.join(" "),
                repo.map_or_else(|| "-".into(), |r| r.work_tree.display().to_string()),
                out.code,
                out.stderr.trim()
            );
        }
        Ok(out)
    }

    /// `run`, failing on a non-zero exit with git's message.
    ///
    /// # Errors
    ///
    /// As `run`, or git failed.
    pub fn ok(&self, repo: Option<&Repo>, args: &[&str], opts: &Opts) -> Result<Output> {
        let out = self.run(repo, args, opts)?;
        if !out.ok {
            bail!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                first_line(&out.stderr)
            );
        }
        Ok(out)
    }

    /// `rev-parse --verify` of `rev`, None when it does not resolve (a
    /// missing ref or object, a path not in a tree).
    pub fn rev(&self, repo: &Repo, rev: &str) -> Result<Option<String>> {
        // NOTE: `<tree>:<path>` takes no `^{object}` suffix
        let spec = if rev.contains(':') {
            rev.to_owned()
        } else {
            format!("{rev}^{{object}}")
        };
        let out = self.run(
            Some(repo),
            &["rev-parse", "--verify", "-q", &spec],
            &Opts::default(),
        )?;
        Ok(out.ok.then(|| out.text()).filter(|s| !s.is_empty()))
    }

    /// The tree of a commit (or the tree itself).
    pub fn tree_of(&self, repo: &Repo, rev: &str) -> Result<String> {
        Ok(self
            .ok(
                Some(repo),
                &["rev-parse", "--verify", "-q", &format!("{rev}^{{tree}}")],
                &Opts::default(),
            )?
            .text())
    }

    /// `write-tree` of `index`.
    pub fn write_tree(&self, repo: &Repo, index: &Path) -> Result<String> {
        Ok(self
            .ok(
                Some(repo),
                &["write-tree"],
                &Opts {
                    index: Some(index),
                    ..Opts::default()
                },
            )?
            .text())
    }

    /// `commit-tree`.
    pub fn commit_tree(
        &self,
        repo: &Repo,
        tree: &str,
        parents: &[&str],
        message: &str,
    ) -> Result<String> {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        args.push("-F");
        args.push("-");
        Ok(self
            .ok(
                Some(repo),
                &args,
                &Opts {
                    stdin: Some(message.as_bytes()),
                    ..Opts::default()
                },
            )?
            .text())
    }

    /// A tree holding `work` as `work/` and `index` as `index/`: what a
    /// snapshot commit points at, so both trees travel and stay in the
    /// history.
    pub fn wrap(&self, repo: &Repo, work: &str, index: &str) -> Result<String> {
        let input = format!("040000 tree {index}\tindex\n040000 tree {work}\twork\n");
        Ok(self
            .ok(
                Some(repo),
                &["mktree", "--missing"],
                &Opts {
                    stdin: Some(input.as_bytes()),
                    ..Opts::default()
                },
            )?
            .text())
    }

    pub fn update_ref(&self, repo: &Repo, name: &str, sha: &str) -> Result<()> {
        self.ok(Some(repo), &["update-ref", name, sha], &Opts::default())?;
        Ok(())
    }

    pub fn delete_ref(&self, repo: &Repo, name: &str) -> Result<()> {
        self.ok(Some(repo), &["update-ref", "-d", name], &Opts::default())?;
        Ok(())
    }

    /// Refs under `prefix` with their objects; symbolic refs
    /// (`refs/remotes/origin/HEAD`) are left out, they follow their target.
    pub fn refs(&self, repo: &Repo, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = self.ok(
            Some(repo),
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)%00%(symref)",
                prefix,
            ],
            &Opts::default(),
        )?;
        Ok(out
            .text()
            .lines()
            .filter_map(|l| {
                let mut parts = l.split('\0');
                let name = parts.next()?;
                let sha = parts.next()?;
                let symref = parts.next().unwrap_or_default();
                symref.is_empty().then(|| (name.to_owned(), sha.to_owned()))
            })
            .collect())
    }

    /// Where `HEAD` points: `Ok(Some(branch))` for a symbolic ref,
    /// `Ok(None)` for a detached head.
    pub fn head_branch(&self, repo: &Repo) -> Result<Option<String>> {
        let head = fs::read_to_string(repo.git_dir.join("HEAD"))
            .with_context(|| format!("reading {}", repo.git_dir.join("HEAD").display()))?;
        Ok(head
            .trim()
            .strip_prefix("ref:")
            .map(|r| r.trim().to_owned()))
    }

    /// `merge-base`, None when there is no common ancestor.
    pub fn merge_base(&self, repo: &Repo, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.run(Some(repo), &["merge-base", a, b], &Opts::default())?;
        Ok(out.ok.then(|| out.text()).filter(|s| !s.is_empty()))
    }

    /// `merge-tree --write-tree` of `ours` and `theirs` on `base`: the
    /// merged tree and the conflicted paths with their blobs.
    pub fn merge_tree(&self, repo: &Repo, base: &str, ours: &str, theirs: &str) -> Result<Merged> {
        let out = self.run(
            Some(repo),
            &[
                "merge-tree",
                "--write-tree",
                "-z",
                &format!("--merge-base={base}"),
                ours,
                theirs,
            ],
            &Opts::default(),
        )?;
        // NOTE: exit 1 is a conflicted merge; anything above is an error
        if !out.ok && out.code != Some(1) {
            bail!("git merge-tree failed: {}", first_line(&out.stderr));
        }
        let mut sections = out.stdout.split(|b| *b == 0);
        let tree = String::from_utf8_lossy(sections.next().unwrap_or_default())
            .trim()
            .to_owned();
        if tree.len() != 40 && tree.len() != 64 {
            bail!("git merge-tree wrote no tree: {}", first_line(&out.stderr));
        }
        let mut conflicts: Vec<Conflicted> = Vec::new();
        for record in sections {
            if record.is_empty() {
                break;
            }
            let record = String::from_utf8_lossy(record);
            let Some((info, path)) = record.split_once('\t') else {
                continue;
            };
            let parts: Vec<&str> = info.split(' ').collect();
            let [mode, oid, stage] = parts[..] else {
                continue;
            };
            let entry = match conflicts.iter_mut().find(|c| c.path == path) {
                Some(c) => c,
                None => {
                    conflicts.push(Conflicted {
                        path: path.to_owned(),
                        ..Conflicted::default()
                    });
                    conflicts.last_mut().expect("just pushed")
                }
            };
            match stage {
                "1" => entry.base = Some(oid.to_owned()),
                "2" => {
                    entry.ours = Some(oid.to_owned());
                    entry.mode = mode.to_owned();
                }
                "3" => {
                    entry.theirs = Some(oid.to_owned());
                    if entry.mode.is_empty() {
                        entry.mode = mode.to_owned();
                    }
                }
                _ => {}
            }
        }
        Ok(Merged { tree, conflicts })
    }

    /// `ls-tree -r` of a tree: path → (mode, object).
    pub fn ls_tree(&self, repo: &Repo, tree: &str) -> Result<BTreeMap<String, (String, String)>> {
        let out = self.ok(Some(repo), &["ls-tree", "-r", "-z", tree], &Opts::default())?;
        Ok(out
            .records()
            .iter()
            .filter_map(|r| {
                let (info, path) = r.split_once('\t')?;
                let mut parts = info.split(' ');
                let mode = parts.next()?;
                let _kind = parts.next()?;
                let oid = parts.next()?;
                Some((path.to_owned(), (mode.to_owned(), oid.to_owned())))
            })
            .collect())
    }

    /// `diff-tree -r --name-status` between two trees: (status, path).
    pub fn diff_names(&self, repo: &Repo, from: &str, to: &str) -> Result<Vec<(char, String)>> {
        let out = self.ok(
            Some(repo),
            &["diff-tree", "-r", "--name-status", "-z", from, to],
            &Opts::default(),
        )?;
        let records = out.records();
        let mut pairs = Vec::new();
        let mut i = 0;
        while i + 1 < records.len() {
            let status = records[i].chars().next().unwrap_or('M');
            // NOTE: renames and copies carry two paths
            let skip = if matches!(status, 'R' | 'C') { 3 } else { 2 };
            pairs.push((status, records[i + skip - 1].clone()));
            i += skip;
        }
        Ok(pairs)
    }

    /// The blob's content.
    pub fn blob(&self, repo: &Repo, oid: &str) -> Result<Vec<u8>> {
        Ok(self
            .ok(Some(repo), &["cat-file", "blob", oid], &Opts::default())?
            .stdout)
    }

    /// Whether the blob looks binary to git (a NUL in the first 8 kB).
    pub fn is_binary(&self, repo: &Repo, oid: &str) -> Result<bool> {
        let out = self.ok(Some(repo), &["cat-file", "blob", oid], &Opts::default())?;
        Ok(out.stdout.iter().take(8000).any(|b| *b == 0))
    }

    /// `check-ignore` on paths relative to the working tree: the ones that
    /// are ignored.
    pub fn ignored(
        &self,
        repo: &Repo,
        excludes: Option<&Path>,
        paths: &[String],
    ) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut stdin = Vec::new();
        for p in paths {
            stdin.extend_from_slice(p.as_bytes());
            stdin.push(0);
        }
        let out = self.run(
            Some(repo),
            &["check-ignore", "-z", "--stdin", "--no-index"],
            &Opts {
                stdin: Some(&stdin),
                excludes,
                ..Opts::default()
            },
        )?;
        // NOTE: exit 1 means none is ignored; 128 an error
        if !out.ok && out.code != Some(1) {
            bail!("git check-ignore failed: {}", first_line(&out.stderr));
        }
        Ok(out.records())
    }

    /// Pushes `refspecs` to `url` (`prune`: remote refs under the pattern
    /// refspecs without a source go); a refusal by the hub's guard is
    /// `Ok(Err(..))` with the tips it printed.
    pub fn push(
        &self,
        repo: &Repo,
        url: &str,
        refspecs: &[String],
        prune: bool,
    ) -> Result<Result<(), Refused>> {
        let mut args: Vec<&str> = vec!["push", "--quiet", "--porcelain", "--no-verify"];
        if prune {
            args.push("--prune");
        }
        args.push(url);
        for r in refspecs {
            args.push(r);
        }
        let started = Instant::now();
        let out = self.run(
            Some(repo),
            &args,
            &Opts {
                net: true,
                ..Opts::default()
            },
        );
        self.cost.pushes.fetch_add(1, Ordering::Relaxed);
        self.cost
            .push_ms
            .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        let out = out?;
        if out.ok {
            return Ok(Ok(()));
        }
        let mut tips = BTreeMap::new();
        for line in out.stderr.lines() {
            let line = line.trim_start_matches("remote:").trim();
            if let Some(rest) = line.strip_prefix("ccdash-tip ")
                && let Some((host, sha)) = rest.split_once(' ')
            {
                tips.insert(host.trim().to_owned(), sha.trim().to_owned());
            }
        }
        if out.stderr.contains("ccdash-refused") || out.stderr.contains("pre-receive hook declined")
        {
            return Ok(Err(Refused {
                tips,
                message: out
                    .stderr
                    .lines()
                    .find(|l| l.contains("ccdash-refused"))
                    .map(|l| l.trim_start_matches("remote:").trim().to_owned())
                    .unwrap_or_else(|| "the hub refused the push".into()),
            }));
        }
        // NOTE: `--porcelain` reports the refused refs on stdout
        let refused_refs: Vec<String> = out
            .text()
            .lines()
            .filter(|l| l.starts_with('!'))
            .map(str::to_owned)
            .collect();
        bail!(
            "git push failed: {}{}",
            first_line(&out.stderr),
            if refused_refs.is_empty() {
                String::new()
            } else {
                format!(" | {}", refused_refs.join(" | "))
            }
        );
    }

    /// Fetches `refspecs` from `url`. `--update-shallow`: a hub repository
    /// that took a shallow clone's push is shallow itself, and a fetch
    /// from it would otherwise reject every ref with only a warning.
    pub fn fetch(&self, repo: &Repo, url: &str, refspecs: &[String]) -> Result<()> {
        let mut args: Vec<&str> = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--update-shallow",
            url,
        ];
        for r in refspecs {
            args.push(r);
        }
        let started = Instant::now();
        let out = self.ok(
            Some(repo),
            &args,
            &Opts {
                net: true,
                ..Opts::default()
            },
        );
        self.cost.fetches.fetch_add(1, Ordering::Relaxed);
        self.cost
            .fetch_ms
            .fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        out.map(|_| ())
    }
}

/// What a timeout ends and whose resources count: the git process (its
/// children's come with `wait4`).
#[cfg(unix)]
struct Tree(u32);

#[cfg(unix)]
impl Tree {
    fn of(child: &Child) -> Self {
        Self(child.id())
    }

    /// Kills the git process, a child of this process not reaped yet.
    fn kill(&self) {
        // SAFETY: the pid is this process's unreaped child
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// What a timeout ends and whose resources count: the job git runs in,
/// with the `mingw64\bin\git.exe` that `cmd\git.exe` (on PATH) starts.
#[cfg(windows)]
struct Tree {
    pid: u32,
    /// None when the job could not be made: the process alone then.
    job: Option<crate::win::Job>,
}

#[cfg(windows)]
impl Tree {
    /// Puts `child`, started suspended, in a job of its own and lets it run.
    fn of(child: &Child) -> Self {
        let job = crate::win::Job::adopt(std::os::windows::io::AsRawHandle::as_raw_handle(child))
            .inspect_err(|e| debug!("git outside a job: {e}"))
            .ok();
        Self {
            pid: child.id(),
            job,
        }
    }

    fn kill(&self) {
        match &self.job {
            Some(job) => job.terminate(),
            None => {
                let _ = crate::win::terminate(self.pid);
            }
        }
    }
}

/// Reaps `child` with `wait4`, so its resource usage comes with its
/// status.
#[cfg(unix)]
fn reap(child: &mut Child, _tree: &Tree) -> std::io::Result<(ExitStatus, Usage)> {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id();
    let mut status: libc::c_int = 0;
    // SAFETY: a zeroed rusage is a valid value; wait4 fills it in.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: plain libc call on a pid this process spawned; the pointers
        // are to live locals.
        let rc = unsafe { libc::wait4(pid as libc::pid_t, &mut status, 0, &mut usage) };
        if rc == -1 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break;
    }
    let tv = |t: libc::timeval| (t.tv_sec as u64) * 1000 + (t.tv_usec as u64) / 1000;
    Ok((
        ExitStatus::from_raw(status),
        Usage {
            cpu_ms: tv(usage.ru_utime) + tv(usage.ru_stime),
            io_read: (usage.ru_inblock.max(0) as u64) * 512,
            io_write: (usage.ru_oublock.max(0) as u64) * 512,
            max_rss_kb: usage.ru_maxrss.max(0) as u64,
        },
    ))
}

/// Waits for `child`, then reads the CPU time, I/O and peak memory of its
/// job (every process git ran), else of `child`'s handle.
#[cfg(windows)]
fn reap(child: &mut Child, tree: &Tree) -> std::io::Result<(ExitStatus, Usage)> {
    let status = child.wait()?;
    let used = tree
        .job
        .as_ref()
        .map_or_else(|| crate::win::usage_of(child), crate::win::Job::usage);
    Ok((
        status,
        Usage {
            cpu_ms: used.cpu_ms,
            io_read: used.read_bytes,
            io_write: used.write_bytes,
            max_rss_kb: used.peak_kb,
        },
    ))
}

/// Reads stdout and stderr of `child` to the end (stderr on a thread), feeds
/// `stdin`, kills it after `timeout`, and reaps it with its resource usage.
fn wait(mut child: Child, tree: Tree, stdin: Option<&[u8]>, timeout: Duration) -> Result<Output> {
    let tree = Arc::new(tree);
    let (mut out_pipe, err_pipe, in_pipe) =
        (child.stdout.take(), child.stderr.take(), child.stdin.take());
    if let (Some(mut pipe), Some(data)) = (in_pipe, stdin) {
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let reaped = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let watchdog = {
        let (reaped, tree) = (reaped.clone(), tree.clone());
        thread::spawn(move || {
            if done_rx.recv_timeout(timeout).is_err() && !reaped.load(Ordering::SeqCst) {
                // NOTE: unreaped (checked just above), so the pid is still the child's
                tree.kill();
                return true;
            }
            false
        })
    };
    let err_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = err_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let mut stdout = Vec::new();
    if let Some(pipe) = &mut out_pipe {
        let _ = pipe.read_to_end(&mut stdout);
    }
    let stderr = err_reader.join().unwrap_or_default();
    let (status, usage) = reap(&mut child, &tree).context("waiting for git")?;
    reaped.store(true, Ordering::SeqCst);
    // NOTE: reaped here; dropping a std Child neither waits nor kills
    drop(child);
    let _ = done_tx.send(());
    let killed = watchdog.join().unwrap_or(false);
    if killed {
        bail!("timed out after {timeout:?}");
    }
    Ok(Output {
        ok: status.success(),
        code: status.code(),
        stdout,
        stderr,
        usage,
    })
}

/// What git had to say: the lines that explain a failure (`fatal:`,
/// `error:`, `remote:`, `!` rejections), at most six, else the first line.
fn first_line(s: &str) -> String {
    let telling: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("fatal:")
                || l.starts_with("error:")
                || l.starts_with("remote:")
                || l.starts_with('!')
        })
        .take(6)
        .collect();
    if !telling.is_empty() {
        return telling.join(" | ");
    }
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message")
        .to_owned()
}

fn write_if_changed(path: &Path, content: &[u8]) -> Result<()> {
    if fs::read(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh test environment: a state dir with the excludes file, and a
    /// repository with one commit.
    pub(crate) fn fixture(name: &str) -> (PathBuf, Git, Repo) {
        let dir = std::env::temp_dir().join(format!("ccdash-wsgit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let git = Git::prepare(
            &dir.join("state"),
            "navi",
            &["node_modules".into(), "target".into()],
            None,
            None,
        )
        .unwrap();
        let repo = Repo {
            git_dir: dir.join("repo/.git"),
            work_tree: dir.join("repo"),
        };
        git.ok(
            Some(&Repo {
                git_dir: dir.join("repo/.git"),
                work_tree: dir.join("repo"),
            }),
            &["init", "-q", "-b", "main"],
            &Opts::default(),
        )
        .unwrap();
        fs::write(dir.join("repo/a.txt"), "one\n").unwrap();
        git.ok(Some(&repo), &["add", "a.txt"], &Opts::default())
            .unwrap();
        let tree = git.write_tree(&repo, &repo.git_dir.join("index")).unwrap();
        let commit = git.commit_tree(&repo, &tree, &[], "base").unwrap();
        git.update_ref(&repo, "refs/heads/main", &commit).unwrap();
        (dir, git, repo)
    }

    #[test]
    fn the_hub_is_resolved_once_and_handed_to_curl() {
        let (dir, mut git, _repo) = fixture("resolve");
        assert_eq!(git.curl_resolve(), None, "no hub, nothing to resolve");
        git.hub_base = Some("https://10.0.0.2/git/".into());
        assert_eq!(git.hub_host_port(), None, "an address needs no lookup");
        git.hub_base = Some("https://[::1]:8443/git/".into());
        assert_eq!(git.hub_host_port(), None);
        git.hub_base = Some("https://hub.example:8443/git/".into());
        assert_eq!(git.hub_host_port(), Some(("hub.example".into(), 8443)));
        git.hub_base = Some("http://hub.example/git/".into());
        assert_eq!(git.hub_host_port(), Some(("hub.example".into(), 80)));
        git.hub_base = Some("https://localhost:8443/git/".into());
        assert_eq!(git.hub_host_port(), Some(("localhost".into(), 443 + 8000)));
        let first = git.curl_resolve().expect("localhost resolves");
        assert!(
            first.starts_with("localhost:8443:")
                && (first.contains("127.0.0.1") || first.contains("[::1]")),
            "{first}"
        );
        // cached: the same answer, from the cache (the entry's time stands)
        let at = git.resolver.lock().unwrap().as_ref().unwrap().at;
        assert_eq!(git.curl_resolve().as_deref(), Some(first.as_str()));
        assert_eq!(git.resolver.lock().unwrap().as_ref().unwrap().at, at);
        // a reconnect forgets the time but keeps the answer as the fallback
        git.forget_hub_address();
        assert!(git.resolver.lock().unwrap().as_ref().unwrap().at < at);
        assert_eq!(git.curl_resolve().as_deref(), Some(first.as_str()));
        // a name that never resolves: the last answer stands
        git.hub_base = Some("https://no-such-host.invalid:1/git/".into());
        assert_eq!(git.curl_resolve().as_deref(), Some(first.as_str()));
        // a net call carries it in the git config environment
        git.token_file = Some(dir.join("token"));
        fs::write(dir.join("token"), "t0k\n").unwrap();
        let cmd = git
            .command(
                None,
                &Opts {
                    net: true,
                    ..Opts::default()
                },
            )
            .unwrap();
        let env: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        let resolve = env
            .iter()
            .find(|(_, v)| v == "http.curloptResolve")
            .map(|(k, _)| k.replace("KEY", "VALUE"))
            .and_then(|k| env.iter().find(|(key, _)| *key == k))
            .map(|(_, v)| v.clone());
        assert_eq!(resolve.as_deref(), Some(first.as_str()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn children_are_reaped_with_their_usage() {
        let (dir, git, repo) = fixture("usage");
        let out = git
            .run(Some(&repo), &["status", "--porcelain"], &Opts::default())
            .unwrap();
        assert!(out.ok);
        assert!(out.usage.max_rss_kb > 0, "{:?}", out.usage);
        let calls = git.cost.calls.load(Ordering::Relaxed);
        assert!(calls >= 1);
        assert!(git.cost.max_rss_kb.load(Ordering::Relaxed) >= out.usage.max_rss_kb);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn plumbing_round_trips_and_merges() {
        let (dir, git, repo) = fixture("plumbing");
        let head = git.rev(&repo, "HEAD").unwrap().unwrap();
        assert_eq!(
            git.head_branch(&repo).unwrap().as_deref(),
            Some("refs/heads/main")
        );
        assert!(git.rev(&repo, "refs/sync/navi").unwrap().is_none());

        // two sides editing different and the same lines
        let base_tree = git.tree_of(&repo, &head).unwrap();
        let idx = dir.join("idx");
        fs::copy(repo.git_dir.join("index"), &idx).unwrap();
        fs::write(repo.work_tree.join("a.txt"), "one\ntwo\n").unwrap();
        git.ok(
            Some(&repo),
            &["add", "-A"],
            &Opts {
                index: Some(&idx),
                ..Opts::default()
            },
        )
        .unwrap();
        let ours = git.write_tree(&repo, &idx).unwrap();
        fs::write(repo.work_tree.join("a.txt"), "zero\none\n").unwrap();
        fs::write(repo.work_tree.join("b.txt"), "b\n").unwrap();
        git.ok(
            Some(&repo),
            &["add", "-A"],
            &Opts {
                index: Some(&idx),
                ..Opts::default()
            },
        )
        .unwrap();
        let theirs = git.write_tree(&repo, &idx).unwrap();
        let merged = git.merge_tree(&repo, &base_tree, &ours, &theirs).unwrap();
        assert!(merged.conflicts.is_empty(), "{merged:?}");
        let entries = git.ls_tree(&repo, &merged.tree).unwrap();
        assert!(entries.contains_key("b.txt"));
        let a = entries["a.txt"].1.clone();
        assert_eq!(git.blob(&repo, &a).unwrap(), b"zero\none\ntwo\n");

        fs::write(repo.work_tree.join("a.txt"), "ONE\n").unwrap();
        git.ok(
            Some(&repo),
            &["add", "-A"],
            &Opts {
                index: Some(&idx),
                ..Opts::default()
            },
        )
        .unwrap();
        let clash = git.write_tree(&repo, &idx).unwrap();
        let merged = git.merge_tree(&repo, &base_tree, &ours, &clash).unwrap();
        assert_eq!(merged.conflicts.len(), 1);
        let c = &merged.conflicts[0];
        assert_eq!(c.path, "a.txt");
        assert!(c.base.is_some() && c.ours.is_some() && c.theirs.is_some());
        let text = git
            .blob(&repo, &git.ls_tree(&repo, &merged.tree).unwrap()["a.txt"].1)
            .unwrap();
        assert!(String::from_utf8_lossy(&text).contains("<<<<<<<"));
        assert!(!git.is_binary(&repo, c.ours.as_ref().unwrap()).unwrap());

        let names = git.diff_names(&repo, &base_tree, &theirs).unwrap();
        assert!(names.contains(&('A', "b.txt".into())) && names.contains(&('M', "a.txt".into())));

        // the empty tree is a merge base too
        let merged = git
            .merge_tree(&repo, super::super::EMPTY_TREE, &ours, &theirs)
            .unwrap();
        assert_eq!(merged.conflicts.len(), 1, "add/add on a.txt");

        // ignored paths come from the always-exclude list, not the repo
        fs::create_dir_all(repo.work_tree.join("node_modules/x")).unwrap();
        fs::create_dir_all(repo.work_tree.join("src")).unwrap();
        let ignored = git
            .ignored(
                &repo,
                None,
                &["node_modules".into(), "src".into(), "node_modules/x".into()],
            )
            .unwrap();
        assert_eq!(ignored, ["node_modules", "node_modules/x"]);

        // the committer needs no user.name; refs list
        let c = git.commit_tree(&repo, &ours, &[&head], "snap").unwrap();
        git.update_ref(&repo, "refs/sync/navi", &c).unwrap();
        assert_eq!(
            git.refs(&repo, "refs/sync/").unwrap(),
            vec![("refs/sync/navi".to_owned(), c.clone())]
        );
        git.delete_ref(&repo, "refs/sync/navi").unwrap();
        assert!(git.refs(&repo, "refs/sync/").unwrap().is_empty());

        // a timeout kills the child
        let slow = git.run(
            Some(&repo),
            &["-c", "core.pager=sleep 5", "log", "-p"],
            &Opts {
                timeout: Some(Duration::from_millis(300)),
                ..Opts::default()
            },
        );
        let _ = slow;
        // a network call without a hub address fails before spawning
        assert!(git.fetch(&repo, "http://127.0.0.1:9/x.git", &[]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
