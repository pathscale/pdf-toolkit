//! Binary-owned, opt-in resource parameters. No argument is removed or reinterpreted.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

const NAMES: [&str; 11] = [
    "rss",
    "cpu-secs",
    "wall-secs",
    "open-files",
    "procs",
    "threads",
    "input-bytes",
    "iterations",
    "cache-entries",
    "decoded-bytes",
    "pixels",
];
struct Budget {
    hard: u64,
    warn: u64,
    warned: AtomicBool,
}
pub struct Caps {
    budgets: BTreeMap<&'static str, Budget>,
    started: Instant,
    stage: Mutex<Arc<str>>,
}
static STATE: OnceLock<Arc<Caps>> = OnceLock::new();
static TURNS: AtomicU64 = AtomicU64::new(0);
static OWNED: Mutex<Vec<u32>> = Mutex::new(Vec::new());
fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("CAP_ERROR {message}");
    stop_owned();
    std::process::exit(78)
}
fn number(value: &str) -> u64 {
    let (digits, scale) = match value.as_bytes().last().copied() {
        Some(b'K' | b'k') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'M' | b'm') => (&value[..value.len() - 1], 1u64 << 20),
        Some(b'G' | b'g') => (&value[..value.len() - 1], 1u64 << 30),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0 && *n < u64::MAX - 1)
        .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER {value:?}")))
}
/// APP_CAP_PROFILE=default opts into the defaults supplied by this main.
/// Otherwise only APP_CAP_MAX_<NAME> selects a cap; APP_CAP_WARN_<NAME> adjusts its warning.
pub fn start(values: [u64; 9]) -> Arc<Caps> {
    let defaults = [
        ("rss", values[0]),
        ("cpu-secs", values[1]),
        ("wall-secs", values[2]),
        ("open-files", values[3]),
        ("procs", values[4]),
        ("threads", values[5]),
        ("input-bytes", values[6]),
        ("iterations", values[7]),
        ("cache-entries", values[8]),
    ];
    let profile = std::env::var_os("APP_CAP_PROFILE");
    if profile.as_deref().is_some_and(|s| s != "default") {
        fail("CAP_BAD_PARAMETER APP_CAP_PROFILE");
    }
    let mut budgets = BTreeMap::new();
    for name in NAMES {
        let key = name.replace('-', "_").to_uppercase();
        let maximum = std::env::var(format!("APP_CAP_MAX_{key}"));
        let warning = std::env::var(format!("APP_CAP_WARN_{key}"));
        let hard = maximum.ok().map(|s| number(&s)).or_else(|| {
            profile
                .as_ref()
                .and_then(|_| defaults.iter().find(|(n, _)| *n == name).map(|(_, n)| *n))
        });
        if let Some(hard) = hard {
            let warn = warning
                .ok()
                .map(|s| number(&s))
                .unwrap_or((hard / 2).max(1));
            if hard == 0 || warn > hard {
                fail(format!("CAP_BAD_PARAMETER {name}: require 0 < warn <= max"));
            }
            budgets.insert(
                name,
                Budget {
                    hard,
                    warn,
                    warned: AtomicBool::new(false),
                },
            );
        } else if warning.is_ok() {
            fail(format!("CAP_BAD_PARAMETER warn-{name} needs max-{name}"));
        }
    }
    let caps = Arc::new(Caps {
        budgets,
        started: Instant::now(),
        stage: Mutex::new(Arc::from("startup")),
    });
    STATE
        .set(Arc::clone(&caps))
        .unwrap_or_else(|_| fail("CAP_ALREADY_STARTED"));
    caps.census();
    caps
}
fn current() -> &'static Arc<Caps> {
    STATE.get().unwrap_or_else(|| fail("CAP_NOT_STARTED"))
}
pub fn hard(name: &str) -> u64 {
    current().budgets.get(name).map_or(u64::MAX - 1, |b| b.hard)
}
pub fn limit(name: &str) -> u64 {
    hard(name)
}
pub fn configured(name: &str) -> bool {
    current().budgets.contains_key(name)
}
pub fn args_os() -> impl Iterator<Item = OsString> {
    std::env::args_os()
}
pub fn args() -> impl Iterator<Item = String> {
    std::env::args()
}
pub fn stage(value: impl Into<Arc<str>>) {
    *current().stage.lock().unwrap_or_else(|e| e.into_inner()) = value.into();
    current().census();
}
pub fn check(name: &str, value: u64) {
    current().census();
    current().check(name, value);
}
pub fn turn(value: impl Into<Arc<str>>) {
    stage(value);
    check(
        "iterations",
        TURNS.fetch_add(1, Ordering::Relaxed).saturating_add(1),
    );
}
impl Caps {
    fn check(&self, name: &str, value: u64) {
        let Some(budget) = self.budgets.get(name) else {
            return;
        };
        if value >= budget.warn && !budget.warned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "CAP_WARN_{} current={value} threshold={} stage={:?}",
                name.replace('-', "_").to_uppercase(),
                budget.warn,
                self.stage.lock().unwrap_or_else(|e| e.into_inner())
            );
        }
        if value > budget.hard {
            fail(format!(
                "CAP_{}_EXCEEDED current={value} threshold={} stage={:?}",
                name.replace('-', "_").to_uppercase(),
                budget.hard,
                self.stage.lock().unwrap_or_else(|e| e.into_inner())
            ));
        }
    }
    fn census(&self) {
        self.check("wall-secs", self.started.elapsed().as_secs());
        if NAMES[..6]
            .iter()
            .filter(|n| **n != "wall-secs")
            .any(|n| self.budgets.contains_key(n))
        {
            let values =
                resources().unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")));
            for (name, value) in values {
                self.check(name, value);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn resources() -> std::io::Result<[(&'static str, u64); 5]> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |key: &str| -> std::io::Result<u64> {
        status
            .lines()
            .find_map(|s| s.strip_prefix(key))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("missing process metric"))
    };
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let tail = stat
        .rsplit_once(')')
        .ok_or_else(|| std::io::Error::other("invalid process stat"))?
        .1;
    let mut columns = tail.split_whitespace().skip(11);
    let user: u64 = columns
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("missing CPU user ticks"))?;
    let system: u64 = columns
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("missing CPU system ticks"))?;
    unsafe extern "C" {
        fn sysconf(name: i32) -> std::ffi::c_long;
    }
    let ticks = unsafe { sysconf(2) };
    if ticks <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let text = std::fs::read_to_string(task?.path().join("children"))?;
        for pid in text
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            children.insert(pid);
        }
    }
    let files = std::fs::read_dir("/proc/self/fd")?
        .count()
        .saturating_sub(1) as u64;
    Ok([
        ("rss", field("VmRSS:")?.saturating_mul(1024)),
        ("cpu-secs", user.saturating_add(system) / (ticks as u64)),
        ("threads", field("Threads:")?),
        ("open-files", files),
        ("procs", children.len() as u64),
    ])
}
#[cfg(target_os = "macos")]
fn resources() -> std::io::Result<[(&'static str, u64); 5]> {
    #[repr(C)]
    struct TaskInfo {
        virtual_size: u64,
        resident_size: u64,
        total_user: u64,
        total_system: u64,
        threads_user: u64,
        threads_system: u64,
        policy: i32,
        faults: i32,
        pageins: i32,
        cow_faults: i32,
        messages_sent: i32,
        messages_received: i32,
        syscalls_mach: i32,
        syscalls_unix: i32,
        csw: i32,
        threadnum: i32,
        numrunning: i32,
        priority: i32,
    }
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            size: i32,
        ) -> i32;
        fn proc_listchildpids(pid: i32, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    }
    unsafe {
        let pid = std::process::id() as i32;
        let mut info: TaskInfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as i32;
        if proc_pidinfo(pid, 4, 0, (&mut info as *mut TaskInfo).cast(), size) != size {
            return Err(std::io::Error::last_os_error());
        }
        let files = if configured("open-files") {
            let mut descriptors = std::mem::MaybeUninit::<[u8; 8 * 65_537]>::uninit();
            let size = std::mem::size_of_val(&descriptors) as i32;
            let count = proc_pidinfo(pid, 1, 0, descriptors.as_mut_ptr().cast(), size);
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            current().check("open-files", count as u64 / 8);
            if count >= size {
                return Err(std::io::Error::other("descriptor census overflow"));
            }
            count as u64 / 8
        } else {
            0
        };
        let procs = if configured("procs") {
            let mut children = std::mem::MaybeUninit::<[i32; 65_537]>::uninit();
            let size = std::mem::size_of_val(&children) as i32;
            let count = proc_listchildpids(pid, children.as_mut_ptr().cast(), size);
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if count >= size {
                current().check("procs", count as u64 / 4);
                return Err(std::io::Error::other("child census overflow"));
            }
            let children =
                std::slice::from_raw_parts(children.as_ptr().cast::<i32>(), count as usize / 4);
            children.iter().filter(|pid| **pid > 0).count() as u64
        } else {
            0
        };
        Ok([
            ("rss", info.resident_size),
            (
                "cpu-secs",
                info.total_user.saturating_add(info.total_system) / 1_000_000_000,
            ),
            ("threads", info.threadnum as u64),
            ("open-files", files),
            ("procs", procs),
        ])
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resources() -> std::io::Result<[(&'static str, u64); 5]> {
    Err(std::io::Error::other("process caps require macOS or Linux"))
}
pub fn reserve_bytes(additional: u64) {
    if !configured("rss") {
        return;
    }
    let values = resources().unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")));
    let rss = values
        .iter()
        .find(|(n, _)| *n == "rss")
        .map_or(0, |(_, v)| *v);
    check("rss", rss.saturating_add(additional));
}
pub fn read_limited(mut reader: impl Read, maximum: u64) -> std::io::Result<Vec<u8>> {
    let maximum = maximum.min(hard("input-bytes"));
    let mut bytes = Vec::new();
    let mut block = [0u8; 8192];
    loop {
        let size = maximum
            .saturating_sub(bytes.len() as u64)
            .saturating_add(1)
            .min(block.len() as u64) as usize;
        let count = match reader.read(&mut block[..size]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            value => value?,
        };
        if count == 0 {
            return Ok(bytes);
        }
        let next = (bytes.len() as u64).saturating_add(count as u64);
        check("input-bytes", next);
        if next > maximum {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "input byte budget exceeded",
            ));
        }
        reserve_bytes(count as u64);
        bytes
            .try_reserve_exact(count)
            .map_err(std::io::Error::other)?;
        bytes.extend_from_slice(&block[..count]);
    }
}
pub fn read_bytes(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    stage("read file");
    let file = std::fs::File::open(path)?;
    check("input-bytes", file.metadata()?.len());
    read_limited(file, hard("input-bytes"))
}
pub fn read_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    String::from_utf8(read_bytes(path)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub fn read_stdin() -> std::io::Result<String> {
    String::from_utf8(read_limited(std::io::stdin().lock(), hard("input-bytes"))?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub fn child(pid: u32) {
    OWNED.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
}
pub fn reaped(pid: u32) {
    OWNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|p| *p != pid);
}
fn stop_owned() {
    #[cfg(unix)]
    unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        for pid in OWNED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            kill(-(*pid as i32), 9);
        }
    }
}
