#![allow(dead_code)]

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const NAMES: [&str; 9] = [
    "rss",
    "cpu-secs",
    "wall-secs",
    "open-files",
    "procs",
    "threads",
    "input-bytes",
    "iterations",
    "cache-entries",
];
static CPU_HIT: AtomicBool = AtomicBool::new(false);
static OWNED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static STATE: OnceLock<Arc<Caps>> = OnceLock::new();

pub struct Caps {
    hard: [u64; 9],
    warn: [u64; 9],
    warned: [AtomicBool; 9],
    args: Arc<[OsString]>,
    stage: Mutex<Arc<str>>,
    started: Instant,
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("CAP_ERROR {message}");
    std::process::exit(78);
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
        .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER value={value:?}")))
}

/// Configure this binary with APP_CAP_MAX_<NAME> and APP_CAP_WARN_<NAME>.
/// Hyphens become underscores. No config means the legacy invocation is uncapped.
/// APP_CAP_PROFILE=default explicitly selects the defaults supplied by main.
pub fn start(defaults: [u64; 9]) -> Arc<Caps> {
    let profile = std::env::var_os("APP_CAP_PROFILE");
    if profile.as_deref().is_some_and(|s| s != "default") {
        fail("CAP_BAD_PARAMETER APP_CAP_PROFILE requires default");
    }
    let mut hard = [u64::MAX - 1; 9];
    let mut warn = [u64::MAX - 1; 9];
    for (i, name) in NAMES.iter().enumerate() {
        let key = name.replace('-', "_").to_uppercase();
        let maximum = std::env::var(format!("APP_CAP_MAX_{key}"));
        let warning = std::env::var(format!("APP_CAP_WARN_{key}"));
        if let Ok(value) = maximum {
            hard[i] = number(&value);
        } else if profile.is_some() {
            hard[i] = defaults[i];
        } else if warning.is_ok() {
            fail(format!("CAP_BAD_PARAMETER warn-{name} needs max-{name}"));
        }
        if hard[i] != u64::MAX - 1 {
            warn[i] = warning
                .map(|value| number(&value))
                .unwrap_or((hard[i] / 2).max(1));
            if warn[i] > hard[i] {
                fail(format!("CAP_BAD_PARAMETER warn-{name} exceeds max-{name}"));
            }
        }
    }
    let caps = Arc::new(Caps {
        hard,
        warn,
        warned: std::array::from_fn(|_| AtomicBool::new(false)),
        args: std::env::args_os().collect::<Vec<_>>().into(),
        stage: Mutex::new(Arc::from("startup")),
        started: Instant::now(),
    });
    STATE
        .set(Arc::clone(&caps))
        .unwrap_or_else(|_| fail("CAP_ALREADY_STARTED"));
    #[cfg(unix)]
    install_kernel_limits(&hard);
    caps.tick();
    if hard[..6].iter().any(|value| *value != u64::MAX - 1) {
        let runtime = nagoya::runtime::Runtime::new(1);
        drop(runtime.spawn(CensusTimer {
            caps: Arc::clone(&caps),
            deadline: nagoya::time::sleep(Duration::from_secs(1)),
        }));
    }
    caps
}

// A registered nagoya timer wakes the census; there is no sleep-loop thread.
struct CensusTimer {
    caps: Arc<Caps>,
    deadline: nagoya::time::Sleep,
}
impl std::future::Future for CensusTimer {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let timer = self.get_mut();
        if std::pin::Pin::new(&mut timer.deadline).poll(cx).is_ready() {
            timer.caps.tick();
            timer.deadline = nagoya::time::sleep(Duration::from_secs(1));
            let _ = std::pin::Pin::new(&mut timer.deadline).poll(cx);
        }
        std::task::Poll::Pending
    }
}

#[cfg(unix)]
fn install_kernel_limits(hard: &[u64; 9]) {
    unsafe {
        for (resource, requested, margin) in [
            (libc::RLIMIT_CPU, hard[1], 2),
            (libc::RLIMIT_NOFILE, hard[3], 0),
        ] {
            if requested == u64::MAX - 1 {
                continue;
            }
            let mut inherited: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(resource, &mut inherited) != 0 {
                fail(std::io::Error::last_os_error());
            }
            let soft = requested.saturating_add(margin) as libc::rlim_t;
            if soft > inherited.rlim_max {
                continue;
            }
            let limit = libc::rlimit {
                rlim_cur: soft,
                rlim_max: inherited.rlim_max,
            };
            if libc::setrlimit(resource, &limit) != 0 {
                fail(std::io::Error::last_os_error());
            }
        }
        if hard[1] != u64::MAX - 1
            && libc::signal(libc::SIGXCPU, cpu_stop as *const () as libc::sighandler_t)
                == libc::SIG_ERR
        {
            fail("CAP_CPU_SIGNAL_SETUP");
        }
    }
}

#[cfg(unix)]
extern "C" fn cpu_stop(_: libc::c_int) {
    const MESSAGE: &[u8] = b"CAP_MAX_CPU_SECS: source=RLIMIT_CPU\n";
    unsafe {
        libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len());
    }
    CPU_HIT.store(true, Ordering::Relaxed);
}

pub fn current() -> &'static Arc<Caps> {
    STATE.get().unwrap_or_else(|| fail("CAP_NOT_STARTED"))
}
pub fn args_os() -> impl Iterator<Item = OsString> {
    current().args.iter().cloned()
}
pub fn stage(value: impl Into<Arc<str>>) {
    *current()
        .stage
        .lock()
        .unwrap_or_else(|_| fail("CAP_STAGE_LOCK")) = value.into();
    current().tick();
}
pub fn hard(name: &str) -> u64 {
    current().hard[NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| fail("CAP_UNKNOWN_BUDGET"))]
}
pub fn check(name: &str, value: u64) {
    current().tick();
    current().check(
        NAMES
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| fail("CAP_UNKNOWN_BUDGET")),
        value,
    );
}

impl Caps {
    fn check(&self, index: usize, value: u64) {
        if self.hard[index] == u64::MAX - 1 {
            return;
        }
        if value >= self.warn[index] && !self.warned[index].swap(true, Ordering::Relaxed) {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            eprintln!(
                "CAP_WARN_{} current={} threshold={} stage={stage:?}",
                NAMES[index].replace('-', "_").to_uppercase(),
                value,
                self.warn[index]
            );
        }
        if value > self.hard[index] || (index == 1 && CPU_HIT.load(Ordering::Relaxed)) {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            stop_owned();
            fail(format!(
                "CAP_{}_EXCEEDED current={} threshold={} stage={stage:?}",
                NAMES[index].replace('-', "_").to_uppercase(),
                value,
                self.hard[index]
            ));
        }
    }
    fn tick(&self) {
        if self.hard[..6].iter().all(|value| *value == u64::MAX - 1) {
            return;
        }
        let (rss, threads, files, procs) =
            sample().unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")));
        self.check(0, rss);
        self.check(2, self.started.elapsed().as_secs());
        self.check(3, files);
        self.check(4, procs);
        self.check(5, threads);
        #[cfg(unix)]
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
                fail("CAP_CPU_METRICS_UNAVAILABLE");
            }
            self.check(1, (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64);
        }
    }
}

#[cfg(target_os = "macos")]
fn sample() -> std::io::Result<(u64, u64, u64, u64)> {
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let pid = libc::getpid();
        if libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        ) != size
        {
            return Err(std::io::Error::last_os_error());
        }
        let files = if current().hard[3] != u64::MAX - 1 {
            let mut descriptors = std::mem::MaybeUninit::<[u8; 8 * 65_537]>::uninit();
            let size = std::mem::size_of_val(&descriptors) as libc::c_int;
            let count = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                descriptors.as_mut_ptr().cast(),
                size,
            );
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            current().check(3, count as u64 / 8);
            if count >= size {
                return Err(std::io::Error::other("descriptor census overflow"));
            }
            count as u64 / 8
        } else {
            0
        };
        let procs = if current().hard[4] != u64::MAX - 1 {
            let mut children = std::mem::MaybeUninit::<[i32; 65_537]>::uninit();
            let size = std::mem::size_of_val(&children) as libc::c_int;
            let count = libc::proc_listchildpids(pid, children.as_mut_ptr().cast(), size);
            if count < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if count >= size {
                current().check(4, count as u64 / 4);
                return Err(std::io::Error::other("child census overflow"));
            }
            let children =
                std::slice::from_raw_parts(children.as_ptr().cast::<i32>(), count as usize / 4);
            children.iter().filter(|pid| **pid > 0).count() as u64
        } else {
            0
        };
        Ok((
            info.pti_resident_size,
            info.pti_threadnum as u64,
            files,
            procs,
        ))
    }
}

#[cfg(target_os = "linux")]
fn sample() -> std::io::Result<(u64, u64, u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |key: &str| -> std::io::Result<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("missing process metric"))
    };
    let mut children = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let path = entry?.path().join("children");
        for pid in std::fs::read_to_string(path)?.split_whitespace() {
            children.insert(pid.to_owned());
        }
    }
    Ok((
        field("VmRSS:")? * 1024,
        field("Threads:")?,
        std::fs::read_dir("/proc/self/fd")?
            .count()
            .saturating_sub(1) as u64,
        children.len() as u64,
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample() -> std::io::Result<(u64, u64, u64, u64)> {
    Err(std::io::Error::other(
        "application resident caps require macOS or Linux",
    ))
}

fn stop_owned() {
    #[cfg(unix)]
    for pid in OWNED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        unsafe {
            libc::kill(-*pid, libc::SIGKILL);
        }
    }
}

/// Read into a fixed stack block, validate growth, then reserve exactly the accepted bytes.
/// The extra overflow byte stays on the stack and never enters the output allocation.
pub fn read_limited(mut reader: impl std::io::Read, maximum: u64) -> std::io::Result<Vec<u8>> {
    let maximum = maximum.min(hard("input-bytes"));
    let mut bytes = Vec::new();
    let mut block = [0u8; 8192];
    loop {
        let remaining = maximum.saturating_sub(bytes.len() as u64);
        let size = remaining.saturating_add(1).min(block.len() as u64) as usize;
        let count = match reader.read(&mut block[..size]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
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
pub fn read_stdin() -> std::io::Result<String> {
    String::from_utf8(read_limited(std::io::stdin().lock(), hard("input-bytes"))?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub fn reserve_bytes(additional: u64) {
    if hard("rss") == u64::MAX - 1 {
        return;
    }
    let rss = sample()
        .unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")))
        .0;
    check("rss", rss.saturating_add(additional));
}

pub fn read_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    String::from_utf8(read_bytes(path)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn read_bytes(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    check("input-bytes", file.metadata()?.len());
    read_limited(file, hard("input-bytes"))
}

pub fn args() -> impl Iterator<Item = String> {
    args_os().map(|s| {
        s.into_string()
            .unwrap_or_else(|_| fail("CAP_NON_UTF8_ARGUMENT"))
    })
}
