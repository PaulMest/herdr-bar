//! The title watcher as a real process, against a fake Herdr on a Unix socket.
//!
//! Ports `WatcherProcessTest` and the handoff tests of the Python release's
//! `tests/test_title_watcher.py`.
#![cfg(unix)]

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A short directory under /tmp (macOS socket paths must stay below 104 bytes).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/hbw-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A plugin directory: the manifest plus `bin/herdr-bar`.
fn plugin_root(path: &Path, binary: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(path.join("bin")).unwrap();
    std::fs::write(path.join("herdr-plugin.toml"), "").unwrap();
    let target = path.join("bin").join("herdr-bar");
    match binary {
        None => {
            std::fs::copy(env!("CARGO_BIN_EXE_herdr-bar"), &target).unwrap();
        }
        Some(script) => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&target, script).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::fs::canonicalize(path).unwrap()
}

/// The JSON string starting at `text` (just after its opening quote).
fn json_string(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    let mut pending_high: Option<u32> = None;
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let unit = u32::from_str_radix(&hex, 16).unwrap();
                    if (0xD800..0xDC00).contains(&unit) {
                        pending_high = Some(unit);
                    } else if let Some(high) = pending_high.take() {
                        let code = 0x10000 + ((high - 0xD800) << 10) + (unit - 0xDC00);
                        out.push(char::from_u32(code).unwrap());
                    } else {
                        out.push(char::from_u32(unit).unwrap());
                    }
                }
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// The string value of `"key"` in a request line.
fn field(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{}\"", key))? + key.len() + 2;
    let rest = line[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(json_string(rest))
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A Herdr with one tab and one codex agent; renames update the tab label.
struct FakeHerdr {
    path: PathBuf,
    label: Arc<Mutex<String>>,
    methods: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeHerdr {
    fn new(path: &Path, registered_root: &Path, title: &str) -> FakeHerdr {
        let listener = UnixListener::bind(path).unwrap();
        let label = Arc::new(Mutex::new("1".to_string()));
        let methods = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let plugins = format!(
            r#"{{"result": {{"plugins": [{{"plugin_id": "paulmest.herdr-bar", "enabled": true, "plugin_root": {}}}]}}}}"#,
            quote(&registered_root.to_string_lossy())
        );
        let title = title.to_string();
        let (shared_label, seen, done) = (label.clone(), methods.clone(), stop.clone());
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
                let method = field(&line, "method").unwrap_or_default();
                seen.lock().unwrap().push(method.clone());
                let reply = match method.as_str() {
                    "plugin.list" => plugins.clone(),
                    "session.snapshot" => {
                        let label = shared_label.lock().unwrap().clone();
                        format!(
                            r#"{{"result": {{"snapshot": {{"tabs": [{{"tab_id": "w1:t9", "workspace_id": "w1", "number": 9, "label": {}}}],
                            "panes": [{{"pane_id": "w1:p9", "tab_id": "w1:t9", "terminal_id": "term9"}}],
                            "agents": [{{"pane_id": "w1:p9", "tab_id": "w1:t9", "agent": "codex", "cwd": "/work/app",
                            "agent_session": {{"agent": "codex", "kind": "id", "value": "12345678-1234-1234-1234-123456789abc"}},
                            "terminal_title_stripped": {}}}]}}}}}}"#,
                            quote(&label),
                            quote(&title)
                        )
                        .replace('\n', " ")
                    }
                    "tab.rename" => {
                        *shared_label.lock().unwrap() = field(&line, "label").unwrap_or_default();
                        r#"{"result": {}}"#.to_string()
                    }
                    _ => r#"{"error": {"message": "unsupported"}}"#.to_string(),
                };
                let _ = conn.write_all((reply + "\n").as_bytes());
            }
        });
        FakeHerdr { path: path.to_path_buf(), label, methods, stop, thread: Some(thread) }
    }

    fn label(&self) -> String {
        self.label.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.methods.lock().unwrap().clone()
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

fn wait_for(what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn try_lock(path: &Path) -> Option<File> {
    let file = OpenOptions::new().append(true).create(true).open(path).unwrap();
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    locked.then_some(file)
}

/// Pids of processes whose command line mentions `needle`.
fn processes_mentioning(needle: &str) -> Vec<i32> {
    let output = Command::new("ps").args(["-A", "-o", "pid=", "-o", "args="]).output().unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

/// The test's environment for a herdr-bar process.
fn command(program: &Path, temp: &Path, socket: &Path, root: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("HERDR_SOCKET_PATH", socket)
        .env("HERDR_PLUGIN_STATE_DIR", temp)
        .env("HERDR_PLUGIN_CONFIG_DIR", temp)
        .env("HERDR_AUTO_TITLE_TRANSCRIPT", "true")
        .env("HERDR_PLUGIN_ROOT", root)
        .env("HERDR_BIN_PATH", temp.join("no-herdr"))
        .env("HOME", temp)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null());
    command
}

/// Detached watchers are not our children: stop them through their marker
/// and, failing that, kill whatever still runs from the test's plugin root.
struct StopWatchers {
    state: PathBuf,
    root: PathBuf,
}

impl Drop for StopWatchers {
    fn drop(&mut self) {
        if let Ok(entries) = std::fs::read_dir(self.state.join("title-servers")) {
            for entry in entries.flatten() {
                let _ = File::create(entry.path().join("title-watcher.stopped"));
            }
        }
        let needle = self.root.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !processes_mentioning(&needle).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        for pid in processes_mentioning(&needle) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn real_process_keeps_lock_updates_closed_popup_and_stops() {
    let temp = TempDir::new();
    let base = temp.0.clone();
    let socket = base.join("s");
    let root = plugin_root(&base.join("root"), None);
    let server = FakeHerdr::new(&socket, &root, "Delayed task | app");
    let _watchers = StopWatchers { state: base.clone(), root: root.clone() };
    std::fs::write(base.join("config.json"), r#"{"agent_icons": "none", "auto_titles": true, "tab_status": true}"#).unwrap();
    let binary = root.join("bin").join("herdr-bar");
    let needle = root.to_string_lossy().into_owned();
    let start = || {
        let status = command(&binary, &base, &socket, &root).arg("--start-titles").status().unwrap();
        assert!(status.success(), "{:?}", status);
    };

    start();
    wait_for("the watcher to rename", 5, || server.label() == "○ Delayed task");
    assert_eq!(processes_mentioning(&needle).len(), 1);
    // A second start must see the lock inherited by the worker.
    start();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(processes_mentioning(&needle).len(), 1);
    assert_eq!(server.label(), "○ Delayed task");

    std::fs::write(
        base.join("config.json"),
        r#"{"agent_icons": "none", "auto_titles": true, "tab_status": true, "lowercase_titles": true}"#,
    )
    .unwrap();
    wait_for("the watcher to reload config", 5, || server.label() == "○ delayed task");

    let status = command(&binary, &base, &socket, &root).arg("--stop-titles").status().unwrap();
    assert!(status.success(), "{:?}", status);
    let directory = std::fs::read_dir(base.join("title-servers")).unwrap().next().unwrap().unwrap().path();
    wait_for("the watcher to release its lock after stop", 5, || {
        try_lock(&directory.join("title-watcher.lock")).is_some()
    });
    assert_eq!(server.label(), "delayed task");
}

#[test]
fn replacement_root_hands_off_and_keeps_the_singleton_lock() {
    // Also test_exec_handoff_preserves_singleton_lock: the replacement gets
    // the locked descriptor's number and the lock stays held across exec.
    let temp = TempDir::new();
    let base = temp.0.clone();
    let socket = base.join("s");
    let root = plugin_root(&base.join("old"), None);
    let args = base.join("args");
    let script = format!("#!/bin/sh\necho \"$@\" > '{}.tmp'\nmv '{0}.tmp' '{0}'\nexec sleep 10\n", args.display());
    let new_root = plugin_root(&base.join("new"), Some(&script));
    let server = FakeHerdr::new(&socket, &new_root, "Task | app");
    std::fs::write(base.join("config.json"), r#"{"agent_icons": "none", "auto_titles": true, "tab_status": true}"#).unwrap();

    let child = command(&root.join("bin").join("herdr-bar"), &base, &socket, &root)
        .args(["--watch-titles", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _child = KillOnDrop(child);
    wait_for("the replacement to start", 5, || args.exists());
    let seen = std::fs::read_to_string(&args).unwrap();
    let words: Vec<&str> = seen.split_whitespace().collect();
    assert_eq!(words.len(), 2, "{:?}", seen);
    assert_eq!(words[0], "--watch-titles");
    assert!(words[1].parse::<i32>().unwrap() >= 0, "{:?}", seen);

    let directory = std::fs::read_dir(base.join("title-servers")).unwrap().next().unwrap().unwrap().path();
    assert!(try_lock(&directory.join("title-watcher.lock")).is_none(), "the lock was let go during the handoff");
    let methods = server.methods();
    assert!(methods.iter().any(|m| m == "plugin.list"), "{:?}", methods);
    assert!(!methods.iter().any(|m| m == "session.snapshot" || m == "tab.rename"), "{:?}", methods);
}
