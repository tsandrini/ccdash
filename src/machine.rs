//! What a machine says about itself (uptime and sleep, CPU, memory, disks,
//! battery, its wired network cards) and waking one over the LAN. Read
//! from `/proc`, `/sys` and `getifaddrs` without privileges (on Windows
//! from Win32 calls and `powercfg`): collectors report it every half
//! minute, the hub reads its own.

use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
    time::Duration,
};
#[cfg(unix)]
use std::{ffi::CStr, fs, os::unix::fs::MetadataExt, path::Path};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::util::now_ms;

/// Where magic packets go unless `wake.via` names a port.
pub const WAKE_PORT: u16 = 9;
/// A clock gap shorter than this is noise, not a sleep.
const SLEEP_MIN_MS: i64 = 2_000;
/// Magic packets per wake, this far apart: one lost datagram must not
/// leave the machine asleep.
const WAKE_REPEAT: u32 = 3;
const WAKE_GAP: Duration = Duration::from_millis(300);

/// How a machine is doing, as it last said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct MachineReport {
    /// When it was read.
    pub at: i64,
    pub boot_at: i64,
    /// Time spent asleep since the boot.
    pub slept_ms: i64,
    /// The last resume the reporter saw; it only sees those while it runs.
    #[serde(default)]
    pub resumed_at: Option<i64>,
    /// `PRETTY_NAME` from os-release.
    #[serde(default)]
    pub os: Option<String>,
    pub kernel: String,
    /// NixOS: the running kernel, initrd or modules are not the current
    /// system's. None elsewhere.
    #[serde(default)]
    pub reboot_pending: Option<bool>,
    pub cpu: Cpu,
    pub memory: Memory,
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub battery: Option<Battery>,
    /// Physical wired network cards.
    #[serde(default)]
    pub nics: Vec<Nic>,
    /// The collector suspends the machine when the hub asks (`[power]
    /// suspend`).
    #[serde(default)]
    pub can_suspend: bool,
    /// The collector locks the user's graphical session when the hub asks
    /// (`[power] lock`).
    #[serde(default)]
    pub can_lock: bool,
    /// Whether the user's graphical session is locked (logind's
    /// `LockedHint`, which the screen locker sets); None without one.
    #[serde(default)]
    pub locked: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Cpu {
    pub cores: u32,
    /// Busy share of all cores since the previous reading; None on the first.
    pub usage_pct: Option<f64>,
    /// Load average over 1, 5 and 15 minutes.
    pub load: [f64; 3],
    /// Package temperature (`k10temp` Tctl, `coretemp` package,
    /// `cpu_thermal`), when a known sensor is there.
    pub temp_c: Option<f64>,
}

/// Bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Memory {
    pub total: u64,
    /// Total minus available: what the page cache cannot give back.
    pub used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

/// One file system, in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Disk {
    /// The path it was measured at.
    pub path: String,
    pub total: u64,
    pub used: u64,
    /// What an unprivileged user may still write.
    pub avail: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Battery {
    pub pct: f64,
    /// The kernel's word: `Charging`, `Discharging`, `Full`, `Not charging`.
    pub status: String,
    /// On mains power; None without a mains supply to ask.
    pub ac: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Nic {
    pub name: String,
    pub mac: String,
    /// `address/prefix`.
    pub ipv4: Vec<String>,
    /// The card may wake the machine (sysfs `power/wakeup`), which is what
    /// enabling wake-on-LAN turns on.
    pub wake: bool,
}

/// What the panel may ask of a machine's power.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Suspend,
    Lock,
}

impl PowerAction {
    pub fn name(self) -> &'static str {
        match self {
            Self::Suspend => "suspend",
            Self::Lock => "lock",
        }
    }
}

/// Asleep or waking, as far as the hub knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Power {
    /// Suspended from the panel.
    Asleep { since: i64 },
    /// Magic packets went out at `since`.
    Waking { since: i64 },
}

/// Whether the hub can wake a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wake {
    /// Magic packets for `mac` go to `to` (`address:port`).
    Ready {
        mac: String,
        to: String,
    },
    Unavailable {
        why: String,
    },
}

/// Reads reports; keeps what a reading needs from the one before.
pub struct Sampler {
    #[cfg(unix)]
    sys: PathBuf,
    #[cfg(unix)]
    proc: PathBuf,
    /// Measured with `statvfs`; one per file system.
    disks: Vec<PathBuf>,
    can_suspend: bool,
    can_lock: bool,
    /// Read the user's graphical session (a collector; the hub has none).
    session: bool,
    cpu: Option<(u64, u64)>,
    /// `CLOCK_BOOTTIME - CLOCK_MONOTONIC` at the last look: time asleep
    /// (on Windows the tick count minus the unbiased interrupt time).
    slept_ms: i64,
    resumed_at: Option<i64>,
}

impl Sampler {
    pub fn new(disks: Vec<PathBuf>) -> Self {
        Self {
            #[cfg(unix)]
            sys: "/sys".into(),
            #[cfg(unix)]
            proc: "/proc".into(),
            disks,
            can_suspend: false,
            can_lock: false,
            session: false,
            cpu: None,
            slept_ms: slept_ms(),
            resumed_at: None,
        }
    }

    /// A collector's sampler: it says what the panel may do here and reads
    /// whether the user's graphical session is locked.
    pub fn collector(mut self, can_suspend: bool, can_lock: bool) -> Self {
        self.can_suspend = can_suspend;
        self.can_lock = can_lock;
        self.session = true;
        self
    }

    /// How long the machine slept since the last look, if it did.
    pub fn woke(&mut self) -> Option<i64> {
        let now = slept_ms();
        let slept = now - self.slept_ms;
        self.slept_ms = now;
        (slept >= SLEEP_MIN_MS).then(|| {
            self.resumed_at = Some(now_ms());
            slept
        })
    }

    /// The busy share of the CPUs since the previous reading; keeps
    /// `ticks` (busy and total) for the next.
    fn usage(&mut self, ticks: Option<(u64, u64)>) -> Option<f64> {
        let usage_pct = match (self.cpu, ticks) {
            (Some((busy0, total0)), Some((busy, total))) if total > total0 => {
                Some(busy.saturating_sub(busy0) as f64 * 100.0 / (total - total0) as f64)
            }
            _ => None,
        };
        if ticks.is_some() {
            self.cpu = ticks;
        }
        usage_pct
    }

    #[cfg(unix)]
    pub fn read(&mut self) -> MachineReport {
        self.woke();
        let now = now_ms();
        let stat = fs::read_to_string(self.proc.join("stat")).unwrap_or_default();
        let usage_pct = self.usage(cpu_ticks(&stat));
        let booted = boottime_ms();
        MachineReport {
            at: now,
            boot_at: now - booted,
            slept_ms: self.slept_ms,
            resumed_at: self.resumed_at,
            os: fs::read_to_string("/etc/os-release")
                .ok()
                .and_then(|t| os_name(&t)),
            kernel: kernel_release(),
            reboot_pending: reboot_pending(Path::new("/run")),
            cpu: Cpu {
                cores: cores(),
                usage_pct,
                load: fs::read_to_string(self.proc.join("loadavg"))
                    .map(|t| load(&t))
                    .unwrap_or_default(),
                temp_c: cpu_temp(&self.sys),
            },
            memory: fs::read_to_string(self.proc.join("meminfo"))
                .map(|t| memory(&t))
                .unwrap_or_default(),
            disks: disks(&self.disks),
            battery: battery(&self.sys),
            nics: nics(&self.sys, &ipv4_addresses().unwrap_or_default()),
            can_suspend: self.can_suspend,
            can_lock: self.can_lock,
            locked: self
                .session
                .then(display_session)
                .flatten()
                .and_then(|s| s.locked),
        }
    }

    /// A report from Win32: no load average or CPU temperature there.
    #[cfg(windows)]
    pub fn read(&mut self) -> MachineReport {
        self.woke();
        let now = now_ms();
        let usage_pct = self.usage(crate::win::cpu_times());
        let (os, kernel) = crate::win::os_version();
        let adapters = crate::win::adapters().unwrap_or_default();
        MachineReport {
            at: now,
            boot_at: now - boottime_ms(),
            slept_ms: self.slept_ms,
            resumed_at: self.resumed_at,
            os,
            kernel,
            reboot_pending: None,
            cpu: Cpu {
                cores: cores(),
                usage_pct,
                load: [0.0; 3],
                temp_c: None,
            },
            memory: memory_status(),
            disks: disks(&self.disks),
            battery: battery(),
            nics: nics(&adapters),
            can_suspend: self.can_suspend,
            can_lock: self.can_lock,
            locked: self
                .session
                .then(display_session)
                .flatten()
                .and_then(|s| s.locked),
        }
    }
}

/// The file system the OS runs from: `/`; on Windows the system drive.
pub fn root_disk() -> PathBuf {
    #[cfg(windows)]
    if let Some(drive) = std::env::var_os("SystemDrive") {
        let mut root = PathBuf::from(drive);
        root.push("\\");
        return root;
    }
    PathBuf::from("/")
}

/// The user's graphical session, as logind knows it (on Windows, this
/// process's Remote Desktop session).
pub struct DisplaySession {
    pub id: String,
    pub locked: Option<bool>,
}

/// The calling user's graphical session (logind's `Display`), asked of
/// `loginctl`; None without one.
#[cfg(unix)]
pub fn display_session() -> Option<DisplaySession> {
    // SAFETY: getuid takes nothing and cannot fail
    let uid = unsafe { libc::getuid() };
    let id = loginctl(&["show-user", &uid.to_string(), "-p", "Display", "--value"])?;
    if id.is_empty() {
        return None;
    }
    let locked = loginctl(&["show-session", &id, "-p", "LockedHint", "--value"]).and_then(|v| {
        match v.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    });
    Some(DisplaySession { id, locked })
}

/// This process's session and whether it is locked.
#[cfg(windows)]
pub fn display_session() -> Option<DisplaySession> {
    let (id, locked) = crate::win::session()?;
    Some(DisplaySession {
        id: id.to_string(),
        locked,
    })
}

#[cfg(unix)]
fn loginctl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("loginctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(unix)]
fn clock_ms(clock: libc::clockid_t) -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec it is handed
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

/// Time since boot, sleep included.
#[cfg(unix)]
fn boottime_ms() -> i64 {
    clock_ms(libc::CLOCK_BOOTTIME)
}

#[cfg(windows)]
fn boottime_ms() -> i64 {
    crate::win::uptime_ms()
}

/// Time spent asleep since boot: `CLOCK_MONOTONIC` stops while suspended,
/// `CLOCK_BOOTTIME` does not.
#[cfg(unix)]
fn slept_ms() -> i64 {
    (boottime_ms() - clock_ms(libc::CLOCK_MONOTONIC)).max(0)
}

#[cfg(windows)]
fn slept_ms() -> i64 {
    crate::win::slept_ms()
}

#[cfg(unix)]
fn cores() -> u32 {
    // SAFETY: sysconf takes and returns plain integers
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u32::try_from(n).unwrap_or(1).max(1)
}

#[cfg(windows)]
fn cores() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1))
}

#[cfg(unix)]
fn kernel_release() -> String {
    // SAFETY: a zeroed utsname is valid; uname fills it with NUL-terminated fields
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return String::new();
    }
    // SAFETY: uname succeeded, so `release` holds a NUL-terminated string
    unsafe { CStr::from_ptr(uts.release.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
/// Busy and total jiffies of all CPUs from `/proc/stat`; iowait counts as idle.
fn cpu_ticks(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|n| n.parse().ok())
        .collect();
    if v.len() < 5 {
        return None;
    }
    // NOTE: guest time is already part of user and nice
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v[4];
    Some((total - idle, total))
}

#[cfg(unix)]
fn load(loadavg: &str) -> [f64; 3] {
    let mut it = loadavg.split_whitespace().map(|n| n.parse().unwrap_or(0.0));
    [
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    ]
}

#[cfg(unix)]
fn memory(meminfo: &str) -> Memory {
    let kb = |key: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let (total, available) = (kb("MemTotal"), kb("MemAvailable"));
    let (swap_total, swap_free) = (kb("SwapTotal"), kb("SwapFree"));
    Memory {
        total,
        used: total.saturating_sub(available),
        swap_total,
        swap_used: swap_total.saturating_sub(swap_free),
    }
}

#[cfg(unix)]
fn os_name(os_release: &str) -> Option<String> {
    let value = os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))?;
    let value = value.trim().trim_matches('"').trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(unix)]
/// NixOS keeps the booted and the current system under `run`.
fn reboot_pending(run: &Path) -> Option<bool> {
    let (booted, current) = (run.join("booted-system"), run.join("current-system"));
    if !booted.exists() || !current.exists() {
        return None;
    }
    Some(["kernel", "initrd", "kernel-modules"].iter().any(|part| {
        fs::canonicalize(booted.join(part)).ok() != fs::canonicalize(current.join(part)).ok()
    }))
}

#[cfg(unix)]
/// Known CPU package sensors, best first: the hwmon name and, when it has
/// several, the label of the one to read.
const SENSORS: &[(&str, Option<&str>)] = &[
    ("k10temp", Some("Tctl")),
    ("zenpower", Some("Tdie")),
    ("coretemp", Some("Package id 0")),
    ("cpu_thermal", None),
];

#[cfg(unix)]
fn cpu_temp(sys: &Path) -> Option<f64> {
    let hwmons: Vec<(String, PathBuf)> = fs::read_dir(sys.join("class/hwmon"))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            Some((read_trim(&dir.join("name"))?, dir))
        })
        .collect();
    SENSORS.iter().find_map(|(name, label)| {
        hwmons
            .iter()
            .filter(|(n, _)| n == name)
            .find_map(|(_, dir)| sensor(dir, *label))
    })
}

#[cfg(unix)]
fn sensor(dir: &Path, label: Option<&str>) -> Option<f64> {
    let input = match label {
        None => dir.join("temp1_input"),
        Some(label) => (1..=64).find_map(|i| {
            (read_trim(&dir.join(format!("temp{i}_label")))?.as_str() == label)
                .then(|| dir.join(format!("temp{i}_input")))
        })?,
    };
    let milli: f64 = read_trim(&input)?.parse().ok()?;
    Some(milli / 1000.0)
}

#[cfg(unix)]
fn battery(sys: &Path) -> Option<Battery> {
    let mut supplies: Vec<PathBuf> = fs::read_dir(sys.join("class/power_supply"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    supplies.sort();
    let ac = supplies
        .iter()
        .filter(|p| read_trim(&p.join("type")).as_deref() == Some("Mains"))
        .find_map(|p| read_trim(&p.join("online")))
        .map(|v| v == "1");
    // NOTE: `scope = Device` is a mouse or a headset, not the machine's battery
    let bat = supplies.iter().find(|p| {
        read_trim(&p.join("type")).as_deref() == Some("Battery")
            && read_trim(&p.join("scope")).as_deref() != Some("Device")
    })?;
    Some(Battery {
        pct: read_trim(&bat.join("capacity"))?.parse().ok()?,
        status: read_trim(&bat.join("status")).unwrap_or_default(),
        ac,
    })
}

#[cfg(unix)]
/// One entry per file system: `/` and `/home` on the same one count once.
fn disks(paths: &[PathBuf]) -> Vec<Disk> {
    let mut seen = HashSet::new();
    paths
        .iter()
        .filter(|p| fs::metadata(p).is_ok_and(|m| seen.insert(m.dev())))
        .filter_map(|p| {
            let st = statvfs(p)?;
            let frsize = st.f_frsize;
            Some(Disk {
                path: p.to_string_lossy().into_owned(),
                total: st.f_blocks * frsize,
                used: st.f_blocks.saturating_sub(st.f_bfree) * frsize,
                avail: st.f_bavail * frsize,
            })
        })
        .collect()
}

#[cfg(unix)]
fn statvfs(path: &Path) -> Option<libc::statvfs> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a zeroed statvfs is valid; statvfs fills it for a NUL-terminated path
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then_some(st)
}

#[cfg(unix)]
/// Physical wired cards: Ethernet (`type` 1) with a device behind them and
/// no wireless side. Bridges, VPNs, containers and Wi-Fi are left out.
fn nics(sys: &Path, addresses: &[(String, Ipv4Addr, u8)]) -> Vec<Nic> {
    let Ok(entries) = fs::read_dir(sys.join("class/net")) else {
        return Vec::new();
    };
    let mut out: Vec<Nic> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if read_trim(&dir.join("type")).as_deref() != Some("1")
                || !dir.join("device").exists()
                || dir.join("wireless").exists()
                || dir.join("phy80211").exists()
            {
                return None;
            }
            let ipv4 = addresses
                .iter()
                .filter(|(n, _, _)| *n == name)
                .map(|(_, ip, prefix)| format!("{ip}/{prefix}"))
                .collect();
            Some(Nic {
                mac: read_trim(&dir.join("address"))?,
                wake: read_trim(&dir.join("device/power/wakeup")).as_deref() == Some("enabled"),
                ipv4,
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Every interface's IPv4 addresses with their prefix length.
///
/// # Errors
///
/// `getifaddrs` failed: it needs a netlink socket, which a sandbox
/// without `AF_NETLINK` refuses.
#[cfg(unix)]
pub fn ipv4_addresses() -> std::io::Result<Vec<(String, Ipv4Addr, u8)>> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates the list it points `head` at, freed below
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: a node of the list getifaddrs returned, alive until freeifaddrs
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: non-null sockaddr pointers of the node; AF_INET ones are sockaddr_in
        let (addr, mask, name) = unsafe {
            if i32::from((*ifa.ifa_addr).sa_family) != libc::AF_INET {
                continue;
            }
            (
                *(ifa.ifa_addr as *const libc::sockaddr_in),
                *(ifa.ifa_netmask as *const libc::sockaddr_in),
                CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned(),
            )
        };
        let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let prefix = u32::from_be(mask.sin_addr.s_addr).count_ones() as u8;
        out.push((name, ip, prefix));
    }
    // SAFETY: the list getifaddrs allocated, not used after this
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

#[cfg(unix)]
fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// Physical memory; swap is the commit charge beyond physical memory,
/// against the page files' size.
#[cfg(windows)]
fn memory_status() -> Memory {
    let Some(m) = crate::win::memory_status() else {
        return Memory::default();
    };
    let used = m.total.saturating_sub(m.avail);
    let swap_total = m.commit_limit.saturating_sub(m.total);
    let committed = m.commit_limit.saturating_sub(m.commit_avail);
    Memory {
        total: m.total,
        used,
        swap_total,
        swap_used: committed.saturating_sub(used).min(swap_total),
    }
}

/// The system battery, in the words sysfs uses.
#[cfg(windows)]
fn battery() -> Option<Battery> {
    let s = crate::win::power_status()?;
    let pct = s.battery_pct?;
    let status = if s.charging {
        "Charging"
    } else if s.ac == Some(false) {
        "Discharging"
    } else if pct >= 100 {
        "Full"
    } else {
        "Not charging"
    };
    Some(Battery {
        pct: f64::from(pct),
        status: status.into(),
        ac: s.ac,
    })
}

/// One entry per volume: `C:\` and the profile on it count once.
#[cfg(windows)]
fn disks(paths: &[PathBuf]) -> Vec<Disk> {
    let mut seen = HashSet::new();
    paths
        .iter()
        .filter(|p| crate::win::volume_of(p).is_some_and(|v| seen.insert(v.to_ascii_lowercase())))
        .filter_map(|p| {
            let space = crate::win::disk_space(p)?;
            Some(Disk {
                path: p.to_string_lossy().into_owned(),
                total: space.total,
                used: space.total.saturating_sub(space.free),
                avail: space.avail,
            })
        })
        .collect()
}

/// Physical wired cards: hardware Ethernet with a connector; virtual
/// switches, VPNs and Wi-Fi are left out. `wake` is the device being armed
/// to wake the machine.
#[cfg(windows)]
fn nics(adapters: &[crate::win::Adapter]) -> Vec<Nic> {
    let armed = wake_armed();
    let mut out: Vec<Nic> = adapters
        .iter()
        .filter(|a| a.wired)
        .filter_map(|a| {
            let mac = a.mac?;
            Some(Nic {
                name: a.name.clone(),
                mac: mac
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":"),
                ipv4: a
                    .ipv4
                    .iter()
                    .map(|(ip, prefix)| format!("{ip}/{prefix}"))
                    .collect(),
                wake: armed.contains(&a.description),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// How long `powercfg`'s answer is kept: arming a card is rare, the query
/// a process.
#[cfg(windows)]
const WAKE_ARMED_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// The devices armed to wake the machine, by name.
#[cfg(windows)]
fn wake_armed() -> HashSet<String> {
    type Cached = Option<(std::time::Instant, HashSet<String>)>;
    static CACHE: std::sync::Mutex<Cached> = std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, armed)) = cache.as_ref()
        && at.elapsed() < WAKE_ARMED_TTL
    {
        return armed.clone();
    }
    let armed = query_wake_armed();
    *cache = Some((std::time::Instant::now(), armed.clone()));
    armed
}

#[cfg(windows)]
fn query_wake_armed() -> HashSet<String> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("powercfg")
        .args(["/devicequery", "wake_armed"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Every adapter's IPv4 addresses with their prefix length.
///
/// # Errors
///
/// `GetAdaptersAddresses` failed.
#[cfg(windows)]
pub fn ipv4_addresses() -> std::io::Result<Vec<(String, Ipv4Addr, u8)>> {
    Ok(crate::win::adapters()?
        .into_iter()
        .flat_map(|a| {
            a.ipv4
                .into_iter()
                .map(move |(ip, prefix)| (a.name.clone(), ip, prefix))
        })
        .collect())
}

/// `aa:bb:cc:dd:ee:ff` (or with `-`).
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let bytes: Vec<u8> = mac
        .split([':', '-'])
        .map(|b| u8::from_str_radix(b, 16).ok().filter(|_| b.len() == 2))
        .collect::<Option<_>>()?;
    bytes.try_into().ok()
}

/// Six `0xff` and the MAC sixteen times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xff; 102];
    for chunk in packet[6..].chunks_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

fn parse_net(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
    Some((ip.parse().ok()?, prefix))
}

fn mask(prefix: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0)
}

fn same_net(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    u32::from(a) & mask(prefix) == u32::from(b) & mask(prefix)
}

fn broadcast(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) | !mask(prefix))
}

/// `ip` or `ip:port`; port 9 when none is given.
fn parse_target(via: &str) -> Option<SocketAddr> {
    via.parse::<SocketAddr>().ok().or_else(|| {
        via.parse::<Ipv4Addr>()
            .ok()
            .map(|ip| SocketAddrV4::new(ip, WAKE_PORT).into())
    })
}

/// Where the hub sends the magic packet for `report`'s machine: the
/// broadcast address of a network the hub shares with a wake-capable card,
/// else `via` (an address the hub routes to, from `[wake] via`).
pub fn wake_path(
    report: Option<&MachineReport>,
    own: &[(String, Ipv4Addr, u8)],
    via: Option<&str>,
) -> Wake {
    let unavailable = |why: String| Wake::Unavailable { why };
    let Some(report) = report else {
        return unavailable("the machine has not reported its network cards yet".into());
    };
    let wakeable: Vec<&Nic> = report.nics.iter().filter(|n| n.wake).collect();
    let Some(first) = wakeable.first() else {
        return unavailable("no wired network card here has wake-on-LAN on".into());
    };
    for nic in &wakeable {
        for (ip, prefix) in nic.ipv4.iter().filter_map(|c| parse_net(c)) {
            let shared = own
                .iter()
                .any(|(_, mine, p)| *p == prefix && same_net(*mine, ip, prefix));
            if shared {
                return Wake::Ready {
                    mac: nic.mac.clone(),
                    to: SocketAddrV4::new(broadcast(ip, prefix), WAKE_PORT).to_string(),
                };
            }
        }
    }
    if let Some(via) = via {
        return match parse_target(via) {
            Some(to) => Wake::Ready {
                mac: first.mac.clone(),
                to: to.to_string(),
            },
            None => unavailable(format!(
                "`wake.via` for this machine is not an address: {via}"
            )),
        };
    }
    let nets: Vec<String> = wakeable
        .iter()
        .flat_map(|n| n.ipv4.iter().filter_map(|c| parse_net(c)))
        .map(|(ip, prefix)| format!("{}/{prefix}", Ipv4Addr::from(u32::from(ip) & mask(prefix))))
        .collect();
    if nets.is_empty() {
        unavailable("its wake-on-LAN card had no IPv4 address when it last reported".into())
    } else {
        unavailable(format!(
            "the hub has no address on {}; `wake.via` can name a routed address",
            nets.join(", ")
        ))
    }
}

/// Keeps the addresses of cards that lost them since `old` (a machine
/// taking its network down on the way to sleep reports none).
pub fn keep_addresses(new: &mut MachineReport, old: &MachineReport) {
    for nic in new.nics.iter_mut().filter(|n| n.ipv4.is_empty()) {
        if let Some(before) = old.nics.iter().find(|o| o.mac == nic.mac) {
            nic.ipv4.clone_from(&before.ipv4);
        }
    }
}

/// Sends the magic packets for `mac` to `to`.
///
/// # Errors
///
/// A bad MAC or address, or a socket that refuses (no route, no broadcast).
pub async fn send_wake(mac: &str, to: &str) -> Result<(), String> {
    let bytes = parse_mac(mac).ok_or_else(|| format!("not a MAC address: {mac}"))?;
    let to: SocketAddr = to.parse().map_err(|_| format!("not an address: {to}"))?;
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| format!("opening a socket: {e}"))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("allowing broadcast: {e}"))?;
    let packet = magic_packet(bytes);
    for i in 0..WAKE_REPEAT {
        if i > 0 {
            tokio::time::sleep(WAKE_GAP).await;
        }
        socket
            .send_to(&packet, to)
            .await
            .map_err(|e| format!("sending to {to}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(name: &str, mac: &str, ipv4: &[&str], wake: bool) -> Nic {
        Nic {
            name: name.into(),
            mac: mac.into(),
            ipv4: ipv4.iter().map(|s| (*s).to_owned()).collect(),
            wake,
        }
    }

    fn report(nics: Vec<Nic>) -> MachineReport {
        MachineReport {
            at: 0,
            boot_at: 0,
            slept_ms: 0,
            resumed_at: None,
            os: None,
            kernel: String::new(),
            reboot_pending: None,
            cpu: Cpu {
                cores: 1,
                usage_pct: None,
                load: [0.0; 3],
                temp_c: None,
            },
            memory: Memory::default(),
            disks: Vec::new(),
            battery: None,
            nics,
            can_suspend: false,
            can_lock: false,
            locked: None,
        }
    }

    fn own() -> Vec<(String, Ipv4Addr, u8)> {
        vec![
            ("lo".into(), Ipv4Addr::LOCALHOST, 8),
            ("br0".into(), Ipv4Addr::new(10, 10, 0, 10), 24),
        ]
    }

    #[test]
    fn a_magic_packet_is_six_ffs_and_the_mac_sixteen_times() {
        let mac = parse_mac("30:56:0f:74:65:de").unwrap();
        assert_eq!(mac, [0x30, 0x56, 0x0f, 0x74, 0x65, 0xde]);
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xff; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
        assert_eq!(parse_mac("30-56-0F-74-65-DE"), Some(mac));
        assert_eq!(parse_mac("30:56:0f:74:65"), None);
        assert_eq!(parse_mac("30:56:0f:74:65:de:00"), None);
        assert_eq!(parse_mac("3:56:0f:74:65:de"), None);
    }

    #[test]
    fn the_hub_wakes_through_a_network_it_shares() {
        let r = report(vec![
            nic("enp1", "aa:aa:aa:aa:aa:aa", &["192.168.31.5/24"], false),
            nic("enp17s0", "30:56:0f:74:65:de", &["10.10.0.204/24"], true),
        ]);
        assert_eq!(
            wake_path(Some(&r), &own(), None),
            Wake::Ready {
                mac: "30:56:0f:74:65:de".into(),
                to: "10.10.0.255:9".into()
            }
        );
        // NOTE: a shared network wins over `via`
        assert!(matches!(
            wake_path(Some(&r), &own(), Some("192.168.31.136")),
            Wake::Ready { ref to, .. } if to == "10.10.0.255:9"
        ));
    }

    #[test]
    fn elsewhere_only_via_reaches_it() {
        let r = report(vec![nic(
            "enp131s0",
            "48:21:0b:7b:de:0d",
            &["192.168.31.136/24"],
            true,
        )]);
        let Wake::Unavailable { why } = wake_path(Some(&r), &own(), None) else {
            panic!("no shared network");
        };
        assert!(why.contains("192.168.31.0/24"), "{why}");
        assert_eq!(
            wake_path(Some(&r), &own(), Some("192.168.31.136")),
            Wake::Ready {
                mac: "48:21:0b:7b:de:0d".into(),
                to: "192.168.31.136:9".into()
            }
        );
        assert!(matches!(
            wake_path(Some(&r), &own(), Some("10.0.33.1:7")),
            Wake::Ready { ref to, .. } if to == "10.0.33.1:7"
        ));
        assert!(matches!(
            wake_path(Some(&r), &own(), Some("vila")),
            Wake::Unavailable { ref why } if why.contains("not an address")
        ));
    }

    #[test]
    fn no_wake_card_no_report_no_wake() {
        let r = report(vec![nic(
            "enp0",
            "aa:bb:cc:dd:ee:ff",
            &["10.10.0.5/24"],
            false,
        )]);
        assert!(matches!(
            wake_path(Some(&r), &own(), Some("10.10.0.5")),
            Wake::Unavailable { ref why } if why.contains("wake-on-LAN on")
        ));
        assert!(matches!(
            wake_path(None, &own(), None),
            Wake::Unavailable { .. }
        ));
        let bare = report(vec![nic("enp0", "aa:bb:cc:dd:ee:ff", &[], true)]);
        assert!(matches!(
            wake_path(Some(&bare), &own(), None),
            Wake::Unavailable { ref why } if why.contains("no IPv4")
        ));
    }

    #[test]
    fn addresses_lost_on_the_way_to_sleep_are_kept() {
        let old = report(vec![nic(
            "enp0",
            "aa:bb:cc:dd:ee:ff",
            &["10.10.0.5/24"],
            true,
        )]);
        let mut new = report(vec![nic("enp0", "aa:bb:cc:dd:ee:ff", &[], true)]);
        keep_addresses(&mut new, &old);
        assert_eq!(new.nics[0].ipv4, ["10.10.0.5/24"]);
    }

    #[test]
    fn network_arithmetic() {
        assert_eq!(
            broadcast(Ipv4Addr::new(10, 10, 0, 204), 24),
            Ipv4Addr::new(10, 10, 0, 255)
        );
        assert_eq!(
            broadcast(Ipv4Addr::new(10, 0, 33, 16), 28),
            Ipv4Addr::new(10, 0, 33, 31)
        );
        assert_eq!(broadcast(Ipv4Addr::new(1, 2, 3, 4), 0), Ipv4Addr::BROADCAST);
        assert!(same_net(
            Ipv4Addr::new(10, 10, 0, 1),
            Ipv4Addr::new(10, 10, 0, 254),
            24
        ));
        assert!(!same_net(
            Ipv4Addr::new(10, 10, 0, 1),
            Ipv4Addr::new(10, 10, 1, 1),
            24
        ));
        assert_eq!(parse_net("10.10.0.204/33"), None);
    }

    #[cfg(unix)]
    #[test]
    fn proc_files_parse() {
        let stat = "cpu  100 5 50 1000 20 3 2 0 0 0\ncpu0 1 2 3 4 5 6 7 8\n";
        assert_eq!(cpu_ticks(stat), Some((160, 1180)));
        assert_eq!(cpu_ticks("intr 1 2 3"), None);
        assert_eq!(load("2.44 2.16 1.93 5/4600 2956962\n"), [2.44, 2.16, 1.93]);
        let mem = memory(concat!(
            "MemTotal:       65536000 kB\nMemFree:  1 kB\nMemAvailable:   49152000 kB\n",
            "SwapTotal:      8388604 kB\nSwapFree:       8388600 kB\n",
        ));
        assert_eq!(mem.total, 65_536_000 * 1024);
        assert_eq!(mem.used, 16_384_000 * 1024);
        assert_eq!(mem.swap_used, 4 * 1024);
        assert_eq!(
            os_name("NAME=NixOS\nPRETTY_NAME=\"NixOS 26.11 (Zokor)\"\n").as_deref(),
            Some("NixOS 26.11 (Zokor)")
        );
        assert_eq!(os_name("NAME=x\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn sysfs_reads_sensors_batteries_and_wired_cards() {
        let root = std::env::temp_dir().join(format!("ccdash-machine-{}", std::process::id()));
        let w = |p: &str, v: &str| {
            let path = root.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, v).unwrap();
        };
        w("class/hwmon/hwmon0/name", "nvme\n");
        w("class/hwmon/hwmon0/temp1_input", "43850\n");
        w("class/hwmon/hwmon1/name", "coretemp\n");
        w("class/hwmon/hwmon1/temp1_label", "Core 0\n");
        w("class/hwmon/hwmon1/temp1_input", "50000\n");
        w("class/hwmon/hwmon1/temp2_label", "Package id 0\n");
        w("class/hwmon/hwmon1/temp2_input", "61500\n");
        assert_eq!(cpu_temp(&root), Some(61.5));
        w("class/hwmon/hwmon2/name", "k10temp\n");
        w("class/hwmon/hwmon2/temp1_label", "Tctl\n");
        w("class/hwmon/hwmon2/temp1_input", "69750\n");
        assert_eq!(cpu_temp(&root), Some(69.75));

        assert_eq!(battery(&root), None);
        w("class/power_supply/hidpp_battery_0/type", "Battery\n");
        w("class/power_supply/hidpp_battery_0/scope", "Device\n");
        w("class/power_supply/hidpp_battery_0/capacity", "10\n");
        w("class/power_supply/AC/type", "Mains\n");
        w("class/power_supply/AC/online", "1\n");
        w("class/power_supply/BAT0/type", "Battery\n");
        w("class/power_supply/BAT0/capacity", "81\n");
        w("class/power_supply/BAT0/status", "Charging\n");
        assert_eq!(
            battery(&root),
            Some(Battery {
                pct: 81.0,
                status: "Charging".into(),
                ac: Some(true)
            })
        );

        w("class/net/enp17s0/type", "1\n");
        w("class/net/enp17s0/address", "30:56:0f:74:65:de\n");
        w("class/net/enp17s0/device/power/wakeup", "enabled\n");
        w("class/net/wlp16s0/type", "1\n");
        w("class/net/wlp16s0/address", "6e:79:bd:bf:91:9b\n");
        w("class/net/wlp16s0/device/power/wakeup", "disabled\n");
        w("class/net/wlp16s0/wireless/x", "");
        w("class/net/docker0/type", "1\n");
        w("class/net/docker0/address", "8a:13:97:bf:fa:51\n");
        w("class/net/tailscale0/type", "65534\n");
        let addrs = vec![
            ("enp17s0".to_owned(), Ipv4Addr::new(10, 10, 0, 204), 24),
            ("docker0".to_owned(), Ipv4Addr::new(172, 17, 0, 1), 16),
        ];
        assert_eq!(
            nics(&root, &addrs),
            vec![nic(
                "enp17s0",
                "30:56:0f:74:65:de",
                &["10.10.0.204/24"],
                true
            )]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn this_machine_reads() {
        let mut s = Sampler::new(vec![root_disk(), root_disk()]).collector(true, false);
        let first = s.read();
        assert!(first.boot_at > 0 && first.boot_at <= first.at);
        assert!(first.cpu.cores >= 1);
        assert_eq!(first.cpu.usage_pct, None);
        assert_eq!(first.disks.len(), 1, "the same file system counts once");
        assert!(first.can_suspend);
        std::thread::sleep(Duration::from_millis(50));
        assert!(s.read().cpu.usage_pct.is_some());
        assert_eq!(s.woke(), None);
    }

    #[cfg(unix)]
    #[test]
    fn reboot_pending_compares_the_systems() {
        let run = std::env::temp_dir().join(format!("ccdash-run-{}", std::process::id()));
        assert_eq!(reboot_pending(&run), None);
        for sys in ["booted-system", "current-system"] {
            fs::create_dir_all(run.join(sys)).unwrap();
            for part in ["kernel", "initrd", "kernel-modules"] {
                let target = run.join(format!("{part}-1"));
                fs::write(&target, "").unwrap();
                std::os::unix::fs::symlink(&target, run.join(sys).join(part)).unwrap();
            }
        }
        assert_eq!(reboot_pending(&run), Some(false));
        let newer = run.join("kernel-2");
        fs::write(&newer, "").unwrap();
        fs::remove_file(run.join("current-system/kernel")).unwrap();
        std::os::unix::fs::symlink(&newer, run.join("current-system/kernel")).unwrap();
        assert_eq!(reboot_pending(&run), Some(true));
        fs::remove_dir_all(&run).unwrap();
    }
}
