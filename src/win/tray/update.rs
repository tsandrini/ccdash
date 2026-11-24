//! Updates from the releases a build names. The release job
//! (`.github/workflows/windows.yml`) keeps one release per protocol number,
//! `windows-p19` for protocol 19, holding the newest build that speaks it:
//! `ccdash-x86_64.exe` and `ccdash-x86_64.json`, the [`Manifest`] of that
//! file. A collector must speak its hub's protocol, so an update follows
//! the hub: the release of the hub's protocol, newer or older than this
//! build, and never one the hub has not reached yet.
//!
//! A build no release job made (plain `cargo build`) names no releases and
//! is left as it is.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use ring::digest;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::protocol;

/// What the release job set for the build; set and empty is not set.
const fn set(value: Option<&'static str>) -> Option<&'static str> {
    match value {
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// Where this build's releases are downloaded from, as
/// `https://github.com/<owner>/<repository>/releases/download`: set by the
/// release job for a build it publishes, which is for the repository the
/// job runs in.
const RELEASES: Option<&str> = set(option_env!("CCDASH_RELEASES"));
/// The commit this build was made from, set by the release job.
pub const REV: Option<&str> = set(option_env!("CCDASH_REV"));

/// The largest executable taken: a few times what ccdash is.
const EXE_LIMIT: u64 = 256 << 20;
const MANIFEST_LIMIT: u64 = 64 << 10;

/// What a release says about its executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The commit it was built from.
    pub rev: String,
    /// The protocol it speaks with the hub.
    pub protocol: u32,
    /// The crate's version.
    pub version: String,
    /// Of the file, in hex.
    pub sha256: String,
    /// The file's, in bytes.
    pub size: u64,
}

/// What the releases hold for a protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// This build is the one released.
    Current,
    /// Another build: the one to run.
    Other(Manifest),
    /// No release speaks that protocol (yet).
    Nothing,
}

/// Whether this build names its releases and its commit, and so updates.
pub fn enabled() -> bool {
    RELEASES.is_some() && REV.is_some()
}

/// A commit as people write it: its first seven characters.
pub fn short(rev: &str) -> &str {
    rev.get(..7).unwrap_or(rev)
}

/// `ccdash 0.1.0 (1a2b3c4), protocol 19`: this build, for a person.
pub fn this_build() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match REV {
        Some(rev) => format!(
            "ccdash {version} ({}), protocol {}",
            short(rev),
            protocol::VERSION
        ),
        None => format!("ccdash {version}, protocol {}", protocol::VERSION),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of a file, in hex, and its size.
///
/// # Errors
///
/// The file cannot be read.
pub fn hash(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        context.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(context.finish().as_ref()), size))
}

/// The manifest of the running executable: what the release job publishes
/// next to it (`ccdash win update --describe`).
///
/// # Errors
///
/// The executable cannot be read.
pub fn describe() -> Result<Manifest> {
    let (sha256, size) = hash(&std::env::current_exe()?)?;
    Ok(Manifest {
        rev: REV.unwrap_or("unknown").to_owned(),
        protocol: protocol::VERSION,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        sha256,
        size,
    })
}

/// The address of a file of the release for `protocol`.
fn address(releases: &str, protocol: u32, extension: &str) -> String {
    format!(
        "{}/windows-p{protocol}/ccdash-{}.{extension}",
        releases.trim_end_matches('/'),
        std::env::consts::ARCH
    )
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

/// What to do about a release's manifest, given this build's commit.
fn compare(manifest: Manifest, protocol: u32, rev: &str) -> Result<Found> {
    if manifest.protocol != protocol {
        bail!(
            "the release for protocol {protocol} holds a build of protocol {}",
            manifest.protocol
        );
    }
    // NOTE: the release holds the newest build of its protocol, so another one is the one to
    // run, whichever was built first
    Ok(if manifest.rev == rev {
        Found::Current
    } else {
        Found::Other(manifest)
    })
}

/// Looks at the release for `protocol`.
///
/// # Errors
///
/// This build names no releases, or they cannot be reached or read.
pub fn check(protocol: u32) -> Result<Found> {
    let (Some(releases), Some(rev)) = (RELEASES, REV) else {
        bail!("this build is not a published release, so it does not update itself");
    };
    let url = address(releases, protocol, "json");
    let response = agent(Duration::from_secs(30))
        .get(&url)
        .call()
        .with_context(|| format!("asking {url}"))?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(Found::Nothing);
    }
    if !status.is_success() {
        bail!("{url} answered {status}");
    }
    let text = response
        .into_body()
        .with_config()
        .limit(MANIFEST_LIMIT)
        .read_to_string()
        .with_context(|| format!("reading {url}"))?;
    let manifest = serde_json::from_str(&text).with_context(|| format!("parsing {url}"))?;
    compare(manifest, protocol, rev)
}

/// Where the running executable is, and the names next to it that an
/// update uses: the download, and where the replaced file goes.
struct Place {
    exe: PathBuf,
    new: PathBuf,
}

impl Place {
    fn here() -> Result<Self> {
        let exe = std::env::current_exe()?;
        let new = with_suffix(&exe, ".new");
        Ok(Self { exe, new })
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Puts `new` where `exe` is. A running executable cannot be written to or
/// removed, but it can be renamed: it goes aside as `<exe>.old-<stamp>`,
/// and the processes running it (each session's `ccdash channel`) keep it.
/// [`sweep`] removes such files once nothing runs them.
///
/// # Errors
///
/// A rename failed; `exe` is then what it was.
pub fn replace(exe: &Path, new: &Path) -> Result<()> {
    if exe.exists() {
        let old = with_suffix(exe, &format!(".old-{}", crate::util::now_ms()));
        std::fs::rename(exe, &old).with_context(|| format!("moving {} aside", exe.display()))?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e).with_context(|| format!("putting {} in place", exe.display()));
        }
    } else {
        std::fs::rename(new, exe).with_context(|| format!("putting {} in place", exe.display()))?;
    }
    Ok(())
}

/// Removes what earlier updates left next to `exe`: replaced executables
/// nothing runs any more, and a download that never got in place.
pub fn sweep(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(rest) = file.to_str().and_then(|f| f.strip_prefix(name)) else {
            continue;
        };
        if rest.starts_with(".old-") || rest == ".new" {
            // NOTE: one a process still runs refuses, and stays for the next sweep
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Downloads the build of `manifest`, checks it against the manifest and
/// asks it what it is, then puts it where the running executable is.
/// Returns that place: the new build is what starts from there now, the
/// running processes go on with the old one.
///
/// # Errors
///
/// No releases, the download failed or is not what the manifest says, or
/// it could not be put in place; the running executable stays then.
pub fn install(manifest: &Manifest) -> Result<PathBuf> {
    let Some(releases) = RELEASES else {
        bail!("this build names no releases");
    };
    let place = Place::here()?;
    let url = address(releases, manifest.protocol, "exe");
    // NOTE: a download that did not arrive is worth another try; one that arrived and is not
    // what the manifest says, or does not run, will be the same the next time
    let fetched =
        fetch(&url, &place.new).and_then(|()| verify(&place.new, manifest).context(Unusable));
    if let Err(e) = fetched {
        let _ = std::fs::remove_file(&place.new);
        return Err(e);
    }
    replace(&place.exe, &place.new)?;
    Ok(place.exe)
}

/// Marks the error of a build that was downloaded and cannot be used: to
/// tell it from a download that failed (`Error::downcast_ref`).
#[derive(Debug)]
pub struct Unusable;

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the downloaded build cannot be used")
    }
}

fn fetch(url: &str, to: &Path) -> Result<()> {
    let response = agent(Duration::from_secs(600))
        .get(url)
        .call()
        .with_context(|| format!("asking {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} answered {status}");
    }
    let mut reader = response
        .into_body()
        .into_with_config()
        .limit(EXE_LIMIT)
        .reader();
    let mut file = File::create(to).with_context(|| format!("creating {}", to.display()))?;
    io::copy(&mut reader, &mut file).with_context(|| format!("downloading {url}"))?;
    file.flush()?;
    Ok(())
}

/// The downloaded file is the manifest's, and runs: asked for its own
/// manifest, it names the same commit and protocol.
fn verify(file: &Path, manifest: &Manifest) -> Result<()> {
    let (sha256, size) = hash(file)?;
    if size != manifest.size || !sha256.eq_ignore_ascii_case(&manifest.sha256) {
        // NOTE: also while the release job replaces the two files, one after the other
        bail!("the download is not the file its manifest describes (the release may be changing)");
    }
    let output = Command::new(file)
        .args(["win", "update", "--describe"])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("starting the downloaded build")?;
    let said: Manifest = serde_json::from_slice(&output.stdout)
        .ok()
        .filter(|_| output.status.success())
        .context("the downloaded build does not say what it is")?;
    if said.rev != manifest.rev || said.protocol != manifest.protocol {
        bail!(
            "the downloaded build is {} of protocol {}, not {} of protocol {}",
            short(&said.rev),
            said.protocol,
            short(&manifest.rev),
            manifest.protocol
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(rev: &str, protocol: u32) -> Manifest {
        Manifest {
            rev: rev.into(),
            protocol,
            version: "0.1.0".into(),
            sha256: "00".into(),
            size: 1,
        }
    }

    #[test]
    fn a_release_is_this_build_or_the_one_to_run() {
        assert_eq!(
            compare(manifest("aaa", 19), 19, "aaa").unwrap(),
            Found::Current
        );
        assert_eq!(
            compare(manifest("bbb", 19), 19, "aaa").unwrap(),
            Found::Other(manifest("bbb", 19))
        );
        assert!(
            compare(manifest("bbb", 20), 19, "aaa").is_err(),
            "a release under the wrong protocol's name is not taken"
        );
    }

    #[test]
    fn a_release_is_named_by_its_protocol_and_the_machine() {
        assert_eq!(
            address("https://example.org/releases/download/", 19, "json"),
            format!(
                "https://example.org/releases/download/windows-p19/ccdash-{}.json",
                std::env::consts::ARCH
            )
        );
    }

    #[test]
    fn a_manifest_reads_what_the_build_writes() {
        let described = describe().unwrap();
        assert_eq!(described.protocol, protocol::VERSION);
        assert_eq!(described.sha256.len(), 64);
        let text = serde_json::to_string(&described).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&text).unwrap(), described);
    }

    #[test]
    fn a_build_that_came_and_cannot_be_used_is_told_from_one_that_did_not_come() {
        let dir = std::env::temp_dir().join(format!("ccdash-unusable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ccdash.exe.new");
        // NOTE: nothing listens there: the download fails, and is worth another try
        let failed = fetch("http://127.0.0.1:9/ccdash-x86_64.exe", &file).unwrap_err();
        assert!(failed.downcast_ref::<Unusable>().is_none(), "{failed:#}");
        // NOTE: what came is not the file the manifest describes: the same the next time
        std::fs::write(&file, "not the build").unwrap();
        let wrong = verify(&file, &manifest("aaa", 19))
            .context(Unusable)
            .unwrap_err();
        assert!(wrong.downcast_ref::<Unusable>().is_some(), "{wrong:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Set for the copy of this test binary that stands for a running ccdash.
    const HOLD: &str = "CCDASH_TEST_HOLD";

    #[test]
    fn a_running_executable_is_replaced_by_moving_it_aside() {
        if std::env::var_os(HOLD).is_some() {
            // NOTE: the copy: runs until its stdin closes (or it is ended)
            let _ = io::stdin().read(&mut [0u8; 1]);
            return;
        }
        let dir = std::env::temp_dir().join(format!("ccdash-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("ccdash.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let mut running = Command::new(&exe)
            .args([
                "--exact",
                "win::tray::update::tests::a_running_executable_is_replaced_by_moving_it_aside",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env(HOLD, "1")
            .spawn()
            .unwrap();
        let new = with_suffix(&exe, ".new");
        std::fs::write(&new, "the new build").unwrap();
        replace(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "the new build");
        assert!(!new.exists());
        let olds = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
                .count()
        };
        assert_eq!(olds(), 1, "the replaced file went aside");
        std::fs::write(&new, "a download that never got in place").unwrap();
        sweep(&exe);
        assert_eq!(olds(), 1, "a file a process runs stays");
        assert!(!new.exists(), "the stray download went");
        std::fs::write(&new, "another").unwrap();
        running.kill().unwrap();
        running.wait().unwrap();
        // NOTE: the file is let go a moment after the process ended
        for _ in 0..50 {
            sweep(&exe);
            if olds() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(olds(), 0, "nothing runs it any more");
        assert!(!new.exists());
        assert!(exe.exists(), "the executable itself stays");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
