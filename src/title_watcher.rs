//! One reconnecting background watcher per Herdr server, for tab titles and status.

use crate::auto_title::{title_state_dir, AutoTitles};
use crate::client::{EventStream, HerdrClient, HerdrError, Kind, Result};
use crate::config::Config;
use crate::host;
use crate::pyjson::Value;
use crate::renaming_guard::{check_plugins, installed_plugins, AutomaticTabClient};
use crate::tab_status::{Shared, TabStatus};
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// Unit tests run the watcher loop in-process; shorter periods keep them fast.
const INTERVAL: f64 = if cfg!(test) { 0.1 } else { 2.0 };
const REGISTRATION_INTERVAL: f64 = if cfg!(test) { 0.5 } else { 10.0 };
const MAX_BACKOFF: f64 = if cfg!(test) { 0.4 } else { 30.0 };
/// In place of an inherited descriptor: the worker claims the lock file by name.
pub const LOCK_BY_NAME: &str = "-";

pub const BINARY: &str = if cfg!(windows) { "herdr-bar.exe" } else { "herdr-bar" };

fn open_append(path: &Path) -> std::result::Result<File, String> {
    host::open_append(path).map_err(|e| host::os_error_path(&e, path))
}

fn mkdir(path: &Path) -> std::result::Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| host::os_error_path(&e, path))
}

/// The plugin directory this binary belongs to: the one Herdr names in
/// `HERDR_PLUGIN_ROOT`, else the nearest ancestor that holds the manifest.
///
/// The variable comes first so that a binary symlinked from, or copied out
/// of, another directory still belongs to the plugin that runs it.
pub fn plugin_root() -> PathBuf {
    if let Some(root) = std::env::var_os("HERDR_PLUGIN_ROOT").filter(|v| !v.is_empty()) {
        let root = PathBuf::from(root);
        if root.join("herdr-plugin.toml").is_file() {
            return std::fs::canonicalize(&root).unwrap_or(root);
        }
    }
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)));
    if let Some(exe) = exe {
        for ancestor in exe.ancestors().skip(1) {
            if ancestor.join("herdr-plugin.toml").is_file() {
                return ancestor.to_path_buf();
            }
        }
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_default()
}

/// Whether two plugin roots name the same directory.
fn same_root(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A copy of `exe` in `directory` to run instead of it, named after its size
/// and modification time so a changed binary gets a fresh copy.
///
/// Windows cannot rename a directory while a process runs an image from it,
/// so a long-lived worker started from the plugin's own binary would make
/// every Herdr update of the plugin fail. The copy is written under a temp
/// name and renamed into place, so a half-written image is never run; older
/// copies are removed when nothing runs them any more.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn staged_binary(exe: &Path, directory: &Path) -> std::io::Result<PathBuf> {
    let meta = std::fs::metadata(exe)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = std::env::consts::EXE_SUFFIX;
    let name = format!("herdr-bar-{}-{}{}", meta.len(), modified, suffix);
    let target = directory.join(&name);
    if !std::fs::metadata(&target).map(|m| m.len() == meta.len()).unwrap_or(false) {
        let temp = directory.join(format!("{}.{}.tmp", name, std::process::id()));
        let copied = std::fs::copy(exe, &temp).and_then(|_| std::fs::rename(&temp, &target));
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            // A racing launcher may have put the same copy in place.
            if !target.is_file() {
                return Err(error);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if file != name && file.starts_with("herdr-bar-") && file.ends_with(suffix) && !file.ends_with(".tmp") {
                // Fails while an older worker still runs it; fine.
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(target)
}

/// How to start the watcher shipped in `root`: its binary, wherever the
/// build step put it, or the Python entry point of an older release.
fn entry_point(root: &Path) -> Vec<OsString> {
    for candidate in [root.join("bin").join(BINARY), root.join("target").join("release").join(BINARY)] {
        if candidate.is_file() {
            return vec![candidate.into_os_string()];
        }
    }
    vec![OsString::from("python3"), root.join("run.py").into_os_string()]
}

/// Best-effort launch; unavailable state storage must never break the popup.
pub fn start(resume: bool) {
    let titles = AutoTitles::new();
    let Some(directory) = titles.state_dir.clone() else { return };
    if std::env::var_os("HERDR_SOCKET_PATH").map(|v| v.is_empty()).unwrap_or(true) {
        return;
    }
    if !titles.enabled && !Config::load().tab_status {
        let records = TabStatus::load(Some(&directory.join("tab-status.json")));
        if !records.iter().any(|(_, record)| !record.get("yielded").truthy()) {
            return;
        }
    }
    let attempt = (|| -> std::result::Result<(), String> {
        mkdir(&directory)?;
        let gate_path = directory.join("title-launch.lock");
        let gate = open_append(&gate_path)?;
        if let Err(error) = host::lock(&gate, resume) {
            if host::is_busy(&error) {
                return Ok(());
            }
            return Err(host::os_error_str(&error));
        }
        launch(&directory, resume)
    })();
    if let Err(message) = attempt {
        eprintln!("herdr-bar: title watcher could not start: {}", message);
    }
}

fn launch(directory: &Path, resume: bool) -> std::result::Result<(), String> {
    let stopped = directory.join("title-watcher.stopped");
    if resume {
        if let Err(error) = std::fs::remove_file(&stopped) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(host::os_error_path(&error, &stopped));
            }
        }
    } else if stopped.exists() {
        return Ok(());
    }
    let lock = open_append(&directory.join("title-watcher.lock"))?;
    if let Err(error) = host::lock(&lock, false) {
        if host::is_busy(&error) {
            return Ok(());
        }
        return Err(host::os_error_str(&error));
    }
    // Keep the singleton lock across spawn and any later exec replacement.
    let log = open_log(directory)?;
    let command = match std::env::current_exe() {
        Ok(exe) => vec![exe.into_os_string()],
        Err(_) => entry_point(&plugin_root()),
    };
    spawn(&command, &plugin_root(), directory, &log, Some(&lock))
}

fn open_log(directory: &Path) -> std::result::Result<File, String> {
    open_append(&directory.join("title-watcher.log"))
}

/// Start a detached worker that holds, or is about to claim, the lock.
///
/// The worker runs in its state directory, not in the plugin's: Windows does
/// not let Herdr replace a directory that a running process works in, so a
/// long-lived worker there would make every plugin update fail.
fn spawn(
    command: &[OsString],
    root: &Path,
    directory: &Path,
    log: &File,
    lock: Option<&File>,
) -> std::result::Result<(), String> {
    #[cfg(windows)]
    let staged: Vec<OsString> = match command {
        [exe] => match staged_binary(Path::new(exe), directory) {
            Ok(copy) => vec![copy.into_os_string()],
            Err(_) => command.to_vec(),
        },
        _ => command.to_vec(),
    };
    #[cfg(windows)]
    let command = &staged[..];
    let token = if host::WINDOWS {
        // CreateProcess cannot hand over a locked handle. Let go so the worker
        // can claim the file by name; a launcher racing into that gap starts a
        // second worker, which loses the claim and exits.
        if let Some(lock) = lock {
            host::unlock(lock);
        }
        LOCK_BY_NAME.to_string()
    } else {
        lock.map(|l| host::handle_number(l).to_string()).unwrap_or_else(|| LOCK_BY_NAME.to_string())
    };
    let mut process = Command::new(&command[0]);
    process.args(&command[1..]).arg("--watch-titles").arg(&token);
    process.current_dir(directory).stdin(Stdio::null()).env("HERDR_PLUGIN_ROOT", root);
    let out = log.try_clone().map_err(|e| host::os_error_str(&e))?;
    let err = log.try_clone().map_err(|e| host::os_error_str(&e))?;
    process.stdout(out).stderr(err);
    if let Some(lock) = lock {
        host::detach(&mut process, lock);
    } else {
        #[cfg(windows)]
        host::detach(&mut process, log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let limit = host::fd_limit();
            unsafe {
                process.pre_exec(move || {
                    libc::setsid();
                    host::cloexec_all_except(-1, limit);
                    Ok(())
                });
            }
        }
    }
    #[cfg(test)]
    if let Some(result) = tests::intercept_spawn(&process) {
        return result;
    }
    process.spawn().map(|_| ()).map_err(|e| {
        let program = PathBuf::from(&command[0]);
        host::os_error_path(&e, &program)
    })
}

pub fn stop() -> std::result::Result<(), String> {
    if let Some(directory) = title_state_dir() {
        mkdir(&directory)?;
        let stopped = directory.join("title-watcher.stopped");
        open_append(&stopped)?;
    }
    Ok(())
}

/// Close the worker lock under the launch gate to make stop/resume atomic.
fn finish_if_stopped(lock: &mut Option<File>, directory: &Path, tabs: Option<&mut TabStatus>) -> bool {
    let stopped = directory.join("title-watcher.stopped");
    if !stopped.exists() {
        return false;
    }
    let Ok(gate) = host::open_append(&directory.join("title-launch.lock")) else {
        return false;
    };
    if host::lock(&gate, true).is_err() {
        return false;
    }
    if stopped.exists() {
        // Still holding the lock, so no successor can decorate meanwhile.
        if let Some(tabs) = tabs {
            tabs.clear();
        }
        *lock = None;
        return true;
    }
    false
}

/// Python's Path.resolve(): follow what exists, keep the rest as written.
fn resolve(path: &str) -> PathBuf {
    let absolute = PathBuf::from(crate::pyos::abspath(path));
    if let Ok(found) = std::fs::canonicalize(&absolute) {
        return found;
    }
    let mut existing = absolute.clone();
    let mut rest: Vec<OsString> = Vec::new();
    while let Some(name) = existing.file_name().map(|n| n.to_os_string()) {
        existing.pop();
        rest.push(name);
        if let Ok(found) = std::fs::canonicalize(&existing) {
            let mut out = found;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
    }
    absolute
}

/// Ask the server, including in named sessions and custom socket locations.
fn registered_root(client: &mut HerdrClient) -> Result<Option<PathBuf>> {
    let plugins = installed_plugins(client)?;
    check_plugins(&plugins, &Config::load().tab_renaming_plugins)?;
    for plugin in &plugins {
        if plugin.get("plugin_id").and_then(|v| v.as_str()) == Some("paulmest.herdr-bar") {
            if !plugin.get("enabled").map(|v| v.truthy()).unwrap_or(false) {
                return Ok(None);
            }
            return match plugin.get("plugin_root") {
                Some(Value::Str(root)) if !root.is_empty() => Ok(Some(resolve(root))),
                _ => Err(HerdrError::new("plugin root missing from plugin.list")),
            };
        }
    }
    Ok(None)
}

type Revision = Vec<(PathBuf, i128, u64)>;

#[cfg(unix)]
fn stamp(meta: &std::fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

#[cfg(windows)]
fn stamp(meta: &std::fs::Metadata) -> i128 {
    use std::os::windows::fs::MetadataExt;
    meta.last_write_time() as i128
}

/// Detect in-place upgrades as well as installs that move the plugin root.
fn revision(root: &Path) -> Result<Revision> {
    let entry = entry_point(root);
    let mut files: Vec<PathBuf> = Vec::new();
    if entry.len() == 1 {
        files.push(PathBuf::from(&entry[0]));
    } else {
        files.push(root.join("run.py"));
        let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join("src").join("herdr_bar"))
            .map(|entries| {
                entries.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "py").unwrap_or(false)).collect()
            })
            .unwrap_or_default();
        sources.sort();
        files.extend(sources);
    }
    let mut out = Vec::new();
    for path in files {
        let meta = std::fs::metadata(&path).map_err(|e| HerdrError::new(host::os_error_path(&e, &path)))?;
        out.push((path, stamp(&meta), meta.len()));
    }
    Ok(out)
}

/// Hand this server's watcher over to the plugin files now registered.
fn replace(root: &Path, lock: &mut Option<File>, directory: &Path) -> Result<()> {
    let command = entry_point(root);
    if host::WINDOWS {
        // No exec on Windows. Release the lock, then start a worker that
        // claims it; a startup hook racing into the gap loses and exits.
        *lock = None;
        let log = open_log(directory).map_err(HerdrError::new)?;
        return spawn(&command, root, directory, &log, None).map_err(HerdrError::new);
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::CommandExt;
        // exec keeps the lock held, so no startup hook can mistake this
        // handoff for a stopped worker, and the replacement cannot race an
        // old writer.
        let fd = lock.as_ref().map(|l| l.as_raw_fd()).unwrap_or(-1);
        // Nothing else this process holds belongs to the replacement.
        host::cloexec_all_except(fd, host::fd_limit());
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
        let error = Command::new(&command[0])
            .args(&command[1..])
            .arg("--watch-titles")
            .arg(fd.to_string())
            .env("HERDR_PLUGIN_ROOT", root)
            .exec();
        Err(HerdrError::os(&error))
    }
    #[cfg(not(unix))]
    {
        Ok(())
    }
}

/// Take the worker lock by name; None when another worker holds it.
fn claim(directory: &Path) -> std::result::Result<Option<File>, String> {
    mkdir(directory)?;
    let lock = open_append(&directory.join("title-watcher.lock"))?;
    match host::lock(&lock, false) {
        Ok(()) => Ok(Some(lock)),
        Err(error) if host::is_busy(&error) => Ok(None),
        Err(error) => Err(host::os_error_str(&error)),
    }
}

#[cfg(unix)]
fn adopt(fd: i64) -> Option<File> {
    use std::os::unix::io::FromRawFd;
    let fd = i32::try_from(fd).ok()?;
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return None;
    }
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    Some(unsafe { File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn adopt(_fd: i64) -> Option<File> {
    None
}

fn monotonic() -> Instant {
    Instant::now()
}

fn secs(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.0))
}

pub fn watch(lock_fd: Option<i64>) -> i32 {
    let client = HerdrClient::new(None, 2.0, true);
    let mut titles = AutoTitles::new();
    let Some(directory) = titles.state_dir.clone() else {
        if let Some(fd) = lock_fd {
            drop(adopt(fd));
        }
        return 0;
    };
    let lock = match lock_fd {
        Some(fd) => match adopt(fd) {
            Some(file) => Some(file),
            None => {
                eprintln!("herdr-bar: title watcher: [Errno 9] Bad file descriptor");
                return 1;
            }
        },
        None => match claim(&directory) {
            Ok(lock) => lock,
            Err(message) => {
                eprintln!("herdr-bar: title watcher: {}", message);
                return 1;
            }
        },
    };
    let mut lock = lock;
    if lock.is_none() {
        return 0;
    }
    #[cfg(windows)]
    {
        // A worker running the plugin's own binary would block Herdr's next
        // update of the plugin. An 0.8 watcher hands over that way, through
        // run.py, so move to a copy in the state directory first.
        let exe = std::env::current_exe().ok();
        let staged = exe.as_deref().is_some_and(|e| e.parent().is_some_and(|p| same_root(p, &directory)));
        if let (Some(exe), false) = (exe.as_deref(), staged) {
            if let Ok(copy) = staged_binary(exe, &directory) {
                drop(lock.take()); // let the copy claim the lock by name
                let started = open_log(&directory)
                    .and_then(|log| spawn(&[copy.into_os_string()], &plugin_root(), &directory, &log, None));
                if started.is_ok() {
                    return 0;
                }
                // Keep watching from here if the copy cannot start.
                lock = claim(&directory).ok().flatten();
                if lock.is_none() {
                    return 0;
                }
            }
        }
    }
    let socket_path = client.socket_path.clone();
    let mut tabs = TabStatus::new(AutomaticTabClient::new(client), Some(directory.clone()));
    // Status changes arrive as events, so idle tabs cost no polling beyond titles.
    let mut events = EventStream::new(socket_path, 2.0);
    let current_root = plugin_root();
    let current_revision = match revision(&current_root) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("herdr-bar: title watcher: {}", error);
            return 1;
        }
    };
    let started = monotonic();
    let at = |offset: f64| started + secs(offset);
    let elapsed = || started.elapsed().as_secs_f64();
    let mut next_registration = 0.0f64;
    let mut next_poll = 0.0f64;
    let mut delay = INTERVAL;
    let mut last_error = String::new();
    loop {
        if finish_if_stopped(&mut lock, &directory, Some(&mut tabs)) {
            return 0;
        }
        let now = elapsed();
        let mut deadline = now + delay;
        let tick = (|| -> Result<Option<i32>> {
            if now >= next_registration {
                let root = registered_root(&mut tabs.client.client)?;
                let Some(root) = root else {
                    tabs.clear();
                    return Ok(Some(0));
                };
                // Same directory and same files: nothing to hand over to.
                if !same_root(&root, &current_root) || revision(&root)? != current_revision {
                    replace(&root, &mut lock, &directory)?;
                    return Ok(Some(0));
                }
                next_registration = now + REGISTRATION_INTERVAL;
            }
            let snapshot: Shared;
            if now >= next_poll {
                let config = Config::load();
                titles.configure(&config);
                tabs.configure(&config);
                if !(titles.enabled || tabs.enabled) {
                    tabs.clear();
                    events.close();
                    return Ok(Some(0));
                }
                let first = tabs.snapshot()?;
                snapshot = titles.apply(&mut tabs, first)?;
                next_poll = now + INTERVAL;
            } else {
                snapshot = tabs.snapshot()?;
            }
            deadline = next_poll;
            tabs.apply(&snapshot);
            if !tabs.client.conflict.is_empty() {
                return Err(HerdrError::renaming(tabs.client.conflict.clone()));
            }
            // e.g. a pane closed since the snapshot; polling covers it
            let _ = events.follow(&tabs.subscriptions());
            delay = INTERVAL;
            last_error.clear();
            Ok(None)
        })();
        match tick {
            Ok(Some(code)) => return code,
            Ok(None) => {}
            Err(error) if error.kind == Kind::Renaming => {
                // Do not even restore our prefixes: another writer owns names now.
                eprintln!("herdr-bar: {}", error);
                events.close();
                return 0;
            }
            Err(error) => {
                // Stay available across server shutdown/restart. Never fall back
                // to a CLI that might address a different server or spawn one.
                let message = error.message.clone();
                if message != last_error {
                    eprintln!("herdr-bar: title watcher retrying: {}", message);
                    last_error = message;
                }
                events.close();
                next_registration = 0.0;
                next_poll = 0.0;
                delay = MAX_BACKOFF.min(delay * 2.0);
                deadline = elapsed() + delay;
            }
        }
        // An explicit stop is noticed even while reconnecting with backoff.
        while monotonic() < at(deadline) {
            if finish_if_stopped(&mut lock, &directory, Some(&mut tabs)) {
                return 0;
            }
            let left = deadline - elapsed();
            if events.wait(left.clamp(0.0, 1.0)) {
                next_poll = 0.0;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, EnvVars, TempDir};
    use std::cell::RefCell;

    /// One worker start that `spawn` was asked for: its arguments and cwd.
    pub struct Spawned {
        pub args: Vec<String>,
        pub cwd: Option<PathBuf>,
    }

    struct Capture {
        calls: Vec<Spawned>,
        fail: bool,
    }

    thread_local! {
        static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
    }

    /// The test seam in `spawn`: while `capture` runs on this thread, record
    /// the worker instead of starting it (or fail as a spawn error would).
    pub fn intercept_spawn(process: &Command) -> Option<std::result::Result<(), String>> {
        CAPTURE.with(|capture| {
            let mut capture = capture.borrow_mut();
            let capture = capture.as_mut()?;
            if capture.fail {
                return Some(Err("spawn failed".to_string()));
            }
            capture.calls.push(Spawned {
                args: process.get_args().map(|a| a.to_string_lossy().into_owned()).collect(),
                cwd: process.get_current_dir().map(Path::to_path_buf),
            });
            Some(Ok(()))
        })
    }

    /// Retry `attempt` until it succeeds, for up to two seconds. A child that
    /// another test forks holds copies of this process's descriptors, locked
    /// files included, until its exec closes them; a released lock can look
    /// held for that moment.
    fn soon<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(found) = attempt() {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The workers a start launches once the lock is free.
    fn start_soon(resume: bool) -> Vec<Spawned> {
        soon(|| Some(capture(false, || start(resume))).filter(|s| !s.is_empty())).unwrap_or_default()
    }

    fn capture(fail: bool, run: impl FnOnce()) -> Vec<Spawned> {
        CAPTURE.with(|c| *c.borrow_mut() = Some(Capture { calls: Vec::new(), fail }));
        run();
        CAPTURE.with(|c| c.borrow_mut().take()).map(|c| c.calls).unwrap_or_default()
    }

    /// WatcherTest.setUp: state, config and socket env in a temp directory,
    /// plus a plugin root of our own so nothing on the machine is touched.
    struct Setup {
        temp: TempDir,
        #[cfg_attr(not(unix), allow(dead_code))]
        root: PathBuf,
        directory: PathBuf,
        _vars: EnvVars,
    }

    impl Setup {
        fn new(socket: Option<&Path>) -> Setup {
            let temp = TempDir::new();
            let base = temp.path().to_path_buf();
            std::fs::write(base.join("config.json"), r#"{"agent_icons": "none", "auto_titles": true, "tab_status": true}"#)
                .unwrap();
            let root = base.join("root");
            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("herdr-plugin.toml"), "").unwrap();
            std::fs::write(root.join("bin").join(BINARY), "binary").unwrap();
            let socket = socket.map(Path::to_path_buf).unwrap_or_else(|| base.join("custom.sock"));
            let mut vars = EnvVars::new();
            vars.set("HERDR_PLUGIN_STATE_DIR", &base)
                .set("HERDR_SOCKET_PATH", &socket)
                .set("HERDR_AUTO_TITLE_TRANSCRIPT", "true")
                .set("HERDR_PLUGIN_CONFIG_DIR", &base)
                .set("HERDR_PLUGIN_ROOT", &root)
                .set("HERDR_BIN_PATH", base.join("no-herdr"))
                .set("HOME", &base)
                .remove("CLAUDE_CONFIG_DIR");
            let directory = title_state_dir().unwrap();
            std::fs::create_dir_all(&directory).unwrap();
            let root = std::fs::canonicalize(&root).unwrap();
            Setup { temp, root, directory, _vars: vars }
        }

        fn config(&self, text: &str) {
            std::fs::write(self.temp.path().join("config.json"), text).unwrap();
        }

        fn lock_file(&self) -> File {
            open_append(&self.directory.join("title-watcher.lock")).unwrap()
        }
    }

    #[test]
    fn default_config_starts_a_watcher() {
        let _env = env_lock();
        let setup = Setup::new(None);
        std::fs::remove_file(setup.temp.path().join("config.json")).unwrap();
        assert_eq!(start_soon(false).len(), 1);
    }

    #[test]
    fn explicit_title_opt_out_does_not_start_a_watcher() {
        let _env = env_lock();
        let setup = Setup::new(None);
        setup.config(r#"{"auto_titles": false}"#);
        assert_eq!(capture(false, || start(false)).len(), 0);
    }

    fn assert_repeated_start_does_not_spawn_another_watcher(setup: &Setup) {
        {
            let lock = setup.lock_file();
            host::lock(&lock, true).unwrap();
            assert_eq!(capture(false, || start(false)).len(), 0);
        }
        let spawns = start_soon(false);
        assert_eq!(spawns.len(), 1);
        // Outside the plugin directory, which Windows could not replace.
        assert_eq!(spawns[0].cwd.as_deref(), Some(setup.directory.as_path()));
        let token = spawns[0].args.last().unwrap();
        assert_eq!(spawns[0].args[spawns[0].args.len() - 2], "--watch-titles");
        if host::WINDOWS {
            assert_eq!(token, LOCK_BY_NAME);
        } else {
            // The locked descriptor itself is handed to the worker.
            assert!(token.parse::<i32>().unwrap() >= 0, "{}", token);
        }
    }

    #[test]
    fn repeated_start_does_not_spawn_another_watcher() {
        let _env = env_lock();
        let setup = Setup::new(None);
        assert_repeated_start_does_not_spawn_another_watcher(&setup);
    }

    #[test]
    fn unwritable_state_or_spawn_failure_does_not_break_popup() {
        let _env = env_lock();
        let setup = Setup::new(None);
        {
            // State under a regular file: every mkdir fails.
            let blocker = setup.temp.path().join("blocker");
            std::fs::write(&blocker, "").unwrap();
            let mut vars = EnvVars::new();
            vars.set("HERDR_PLUGIN_STATE_DIR", blocker.join("state"));
            assert_eq!(capture(false, || start(false)).len(), 0);
        }
        assert_eq!(capture(true, || start(false)).len(), 0);
        // Failed spawn must release the singleton lock.
        assert_repeated_start_does_not_spawn_another_watcher(&setup);
    }

    #[test]
    fn stop_prevents_event_restarts_and_explicit_resume_clears_it() {
        let _env = env_lock();
        let _setup = Setup::new(None);
        stop().unwrap();
        assert_eq!(capture(false, || start(false)).len(), 0);
        assert_eq!(start_soon(true).len(), 1);
    }

    #[test]
    fn stop_resume_interleavings_do_not_lose_the_watcher() {
        let _env = env_lock();
        let setup = Setup::new(None);
        let file = setup.lock_file();
        host::lock(&file, true).unwrap();
        let mut lock = Some(file);
        stop().unwrap();
        let spawns = capture(false, || {
            start(true);
            assert!(!finish_if_stopped(&mut lock, &setup.directory, None));
        });
        assert_eq!(spawns.len(), 0);
        assert!(lock.is_some());
        stop().unwrap();
        assert!(finish_if_stopped(&mut lock, &setup.directory, None));
        assert!(lock.is_none());
        assert_eq!(start_soon(true).len(), 1);
    }

    #[test]
    fn server_scoped_state_separates_named_sessions() {
        let _env = env_lock();
        let setup = Setup::new(None);
        let mut vars = EnvVars::new();
        vars.set("HERDR_SOCKET_PATH", "/other/session.sock");
        assert_ne!(title_state_dir().unwrap(), setup.directory);
    }

    #[test]
    fn windows_handoff_releases_the_lock_for_a_worker_that_claims_it() {
        // The claim half of the by-name handoff; `replace` itself execs on POSIX.
        let _env = env_lock();
        let setup = Setup::new(None);
        let lock = claim(&setup.directory).unwrap();
        assert!(lock.is_some());
        assert!(claim(&setup.directory).unwrap().is_none());
        drop(lock);
        assert!(soon(|| claim(&setup.directory).unwrap()).is_some());
    }

    #[cfg(unix)]
    mod with_server {
        use super::*;
        use crate::pyjson;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::io::IntoRawFd;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::sync::{Arc, Mutex};

        type Responder = dyn Fn(&Value) -> Option<Value> + Send + Sync;

        /// A stand-in for the Herdr socket: one request per connection.
        /// A responder answering None drops the connection unanswered.
        struct FakeHerdr {
            path: PathBuf,
            requests: Arc<Mutex<Vec<Value>>>,
            stop: Arc<AtomicBool>,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        impl FakeHerdr {
            fn new(path: &Path, responder: Arc<Responder>) -> FakeHerdr {
                let listener = UnixListener::bind(path).unwrap();
                let requests = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let (seen, done) = (requests.clone(), stop.clone());
                let thread = std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        if done.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(mut conn) = conn else { continue };
                        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut line = String::new();
                        if BufReader::new(&conn).read_line(&mut line).is_err() {
                            continue;
                        }
                        let Ok(request) = pyjson::loads(&line) else { continue };
                        seen.lock().unwrap().push(request.clone());
                        if let Some(reply) = responder(&request) {
                            let _ = conn.write_all((pyjson::dumps(&reply, false) + "\n").as_bytes());
                        }
                    }
                });
                FakeHerdr { path: path.to_path_buf(), requests, stop, thread: Some(thread) }
            }

            fn methods(&self) -> Vec<String> {
                self.requests.lock().unwrap().iter().map(|r| r.get("method").as_str().unwrap_or("").to_string()).collect()
            }

            fn renames(&self) -> Vec<String> {
                let requests = self.requests.lock().unwrap();
                requests
                    .iter()
                    .filter(|r| r.get("method").as_str() == Some("tab.rename"))
                    .map(|r| r.get("params").get("label").as_str().unwrap_or("").to_string())
                    .collect()
            }
        }

        impl Drop for FakeHerdr {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                let _ = UnixStream::connect(&self.path);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                let _ = std::fs::remove_file(&self.path);
            }
        }

        fn ok(result: Value) -> Option<Value> {
            let mut map = pyjson::Map::new();
            map.insert("result", result);
            Some(Value::Dict(map))
        }

        fn json(text: &str) -> Value {
            pyjson::loads(text).unwrap()
        }

        fn plugins(root: &Path, enabled: bool, extra: &str) -> Value {
            let root = pyjson::dumps(&Value::str(root.to_string_lossy()), false);
            json(&format!(
                r#"{{"plugins": [{{"plugin_id": "paulmest.herdr-bar", "enabled": {}, "plugin_root": {}}}{}]}}"#,
                if enabled { "true" } else { "false" },
                root,
                extra
            ))
        }

        /// test_auto_title.snapshot(): one tab labelled "1" with one agent.
        fn snapshot() -> Value {
            json(
                r#"{"tabs": [{"tab_id": "w1:t9", "workspace_id": "w1", "number": 9, "label": "1"}],
                "panes": [{"pane_id": "w1:p9", "tab_id": "w1:t9", "terminal_id": "term9"}],
                "agents": [{"pane_id": "w1:p9", "tab_id": "w1:t9", "agent": "claude", "cwd": "/work/app",
                "agent_session": {"agent": "claude", "kind": "id", "value": "12345678-1234-1234-1234-123456789abc"}}]}"#,
            )
        }

        /// A Herdr that serves `data` (renames update it) and lists `plugin_list`.
        fn herdr(path: &Path, data: Arc<Mutex<Value>>, plugin_list: Value, fail: Arc<AtomicBool>) -> FakeHerdr {
            FakeHerdr::new(
                path,
                Arc::new(move |request: &Value| {
                    if fail.load(Ordering::SeqCst) {
                        return None;
                    }
                    match request.get("method").as_str().unwrap_or("") {
                        "plugin.list" => ok(plugin_list.clone()),
                        "session.snapshot" => {
                            let mut map = pyjson::Map::new();
                            map.insert("snapshot", data.lock().unwrap().clone());
                            ok(Value::Dict(map))
                        }
                        "tab.rename" => {
                            let label = request.get("params").get("label").clone();
                            let mut data = data.lock().unwrap();
                            if let Some(Value::List(tabs)) = data.as_dict_mut().unwrap().get_mut("tabs") {
                                tabs[0].as_dict_mut().unwrap().insert("label", label);
                            }
                            ok(Value::Dict(pyjson::Map::new()))
                        }
                        _ => Some(json(r#"{"error": {"message": "unsupported"}}"#)),
                    }
                }),
            )
        }

        fn label(data: &Arc<Mutex<Value>>) -> String {
            data.lock().unwrap().get("tabs").list_or_empty()[0].get("label").as_str().unwrap_or("").to_string()
        }

        /// A descriptor for a locked handle, as `--watch-titles N` receives.
        fn lock_fd(setup: &Setup) -> i64 {
            let file = setup.lock_file();
            host::lock(&file, false).unwrap();
            file.into_raw_fd() as i64
        }

        /// Run `watch` on its own thread; a stop marker ends it if the test fails.
        fn watch_in_background(fd: i64) -> (mpsc::Receiver<i32>, StopOnDrop) {
            let (send, receive) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = send.send(watch(Some(fd)));
            });
            (receive, StopOnDrop)
        }

        struct StopOnDrop;

        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    let _ = stop();
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }

        fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {}", what);
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn registration_uses_api_instead_of_socket_directory() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            let reply = Arc::new(Mutex::new(plugins(&setup.root, true, "")));
            let shared = reply.clone();
            let _server = FakeHerdr::new(&socket, Arc::new(move |_: &Value| ok(shared.lock().unwrap().clone())));
            let mut client = HerdrClient::new(None, 2.0, true);
            assert_eq!(registered_root(&mut client).unwrap(), Some(setup.root.clone()));
            *reply.lock().unwrap() = plugins(&setup.root, false, "");
            assert_eq!(registered_root(&mut client).unwrap(), None);
            *reply.lock().unwrap() = json(r#"{"plugins": []}"#);
            assert_eq!(registered_root(&mut client).unwrap(), None);
            *reply.lock().unwrap() = json("{}");
            let error = registered_root(&mut client).unwrap_err();
            assert_eq!(error.kind, Kind::Herdr);
        }

        #[test]
        fn enabled_naming_plugin_prevents_all_tab_operations() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            let list = plugins(&setup.root, true, r#", {"plugin_id": "herdr.auto-title", "enabled": true}"#);
            let server = herdr(&socket, Arc::new(Mutex::new(snapshot())), list, Arc::new(AtomicBool::new(false)));
            let mut client = HerdrClient::new(None, 2.0, true);
            assert_eq!(registered_root(&mut client).unwrap_err().kind, Kind::Renaming);
            assert_eq!(watch(Some(lock_fd(&setup))), 0);
            let methods = server.methods();
            assert!(!methods.iter().any(|m| m == "session.snapshot" || m == "tab.rename"), "{:?}", methods);
        }

        #[test]
        fn disabled_plugin_exits_before_any_tab_operations() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            let list = plugins(&setup.root, false, "");
            let server = herdr(&socket, Arc::new(Mutex::new(snapshot())), list, Arc::new(AtomicBool::new(false)));
            assert_eq!(watch(Some(lock_fd(&setup))), 0);
            let methods = server.methods();
            assert!(methods.iter().any(|m| m == "plugin.list"), "{:?}", methods);
            assert!(!methods.iter().any(|m| m == "session.snapshot" || m == "tab.rename"), "{:?}", methods);
        }

        #[test]
        fn socket_only_reconnects_and_never_falls_back_to_cli() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            // A herdr CLI that leaves a mark if anything runs it.
            let marker = setup.temp.path().join("cli-ran");
            let cli = setup.temp.path().join("herdr");
            std::fs::write(&cli, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut vars = EnvVars::new();
            vars.set("HERDR_BIN_PATH", &cli);
            let mut client = HerdrClient::new(None, 2.0, true);
            // Server stopped: an error, not a CLI call.
            assert!(client.snapshot().is_err());
            let data = Arc::new(Mutex::new(json(r#"{"tabs": []}"#)));
            let server = herdr(&socket, data, json(r#"{"plugins": []}"#), Arc::new(AtomicBool::new(false)));
            let snapshot = client.snapshot().unwrap();
            assert!(pyjson::py_eq(&snapshot, &json(r#"{"tabs": []}"#)));
            assert_eq!(server.methods(), vec!["session.snapshot".to_string()]);
            assert!(!marker.exists(), "the CLI ran");
        }

        #[test]
        fn disabling_both_features_restores_owned_prefix_and_exits() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            setup.config(r#"{"auto_titles": false, "tab_status": true, "agent_icons": "none"}"#);
            let data = Arc::new(Mutex::new(snapshot()));
            let list = plugins(&setup.root, true, "");
            let server = herdr(&socket, data.clone(), list, Arc::new(AtomicBool::new(false)));
            let (result, _guard) = watch_in_background(lock_fd(&setup));
            wait_for("the status prefix", || label(&data) == "○ 1");
            setup.config(r#"{"auto_titles": false, "tab_status": false, "agent_icons": "none"}"#);
            let code = result.recv_timeout(Duration::from_secs(5)).expect("watcher did not exit after both features were disabled");
            assert_eq!(code, 0);
            assert_eq!(server.renames(), vec!["○ 1".to_string(), "1".to_string()]);
        }

        #[test]
        fn watcher_recovers_after_server_restart_without_opening_popup() {
            let _env = env_lock();
            let socket = crate::testutil::socket_path();
            let setup = Setup::new(Some(&socket));
            let mut data = snapshot();
            {
                let agents = data.as_dict_mut().unwrap().get_mut("agents").unwrap();
                let Value::List(agents) = agents else { unreachable!() };
                let agent = agents[0].as_dict_mut().unwrap();
                agent.insert("agent", Value::str("codex"));
                agent.insert("terminal_title_stripped", Value::str("First title | app"));
                agent.get_mut("agent_session").unwrap().as_dict_mut().unwrap().insert("agent", Value::str("codex"));
            }
            let data = Arc::new(Mutex::new(data));
            let fail = Arc::new(AtomicBool::new(false));
            let server = herdr(&socket, data.clone(), plugins(&setup.root, true, ""), fail.clone());
            let (result, _guard) = watch_in_background(lock_fd(&setup));
            wait_for("the first title", || label(&data) == "○ First title");
            // The server goes away for a few backoff rounds...
            fail.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(800));
            {
                let mut data = data.lock().unwrap();
                let Some(Value::List(agents)) = data.as_dict_mut().unwrap().get_mut("agents") else { unreachable!() };
                agents[0].as_dict_mut().unwrap().insert("terminal_title_stripped", Value::str("After restart | app"));
            }
            // ...and comes back with the agent's title changed.
            fail.store(false, Ordering::SeqCst);
            wait_for("the title after restart", || label(&data) == "○ After restart");
            stop().unwrap();
            let code = result.recv_timeout(Duration::from_secs(5)).expect("watcher did not stop");
            assert_eq!(code, 0);
            // Status-led while watched, bare again once stopped.
            assert_eq!(
                server.renames(),
                vec!["○ First title".to_string(), "○ After restart".to_string(), "After restart".to_string()]
            );
        }
    }

    #[test]
    fn staged_binary_is_reused_and_replaced() {
        let base = std::env::temp_dir().join(format!("herdr-bar-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let state = base.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let exe = base.join("source");
        std::fs::write(&exe, b"one").unwrap();
        let first = staged_binary(&exe, &state).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(staged_binary(&exe, &state).unwrap(), first);
        std::fs::write(&exe, b"two, longer").unwrap();
        let second = staged_binary(&exe, &state).unwrap();
        assert_ne!(second, first);
        assert_eq!(std::fs::read(&second).unwrap(), b"two, longer");
        // The stale copy is gone and no temp file is left behind.
        let names: Vec<_> = std::fs::read_dir(&state).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(names, vec![second]);
        assert!(staged_binary(&base.join("missing"), &state).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn roots_compare_through_links() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(same_root(&base, &std::env::temp_dir()));
        assert!(same_root(&base, &base.join(".")));
        assert!(!same_root(&base, &base.join("herdr-bar-no-such-dir")));
    }
}
