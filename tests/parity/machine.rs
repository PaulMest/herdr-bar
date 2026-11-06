//! The machine every run sees (parity.py's Machine and fake tools), and the
//! plugin on a pseudo-terminal (bench_process.py's Session).

use crate::herdr::Herdr;
use crate::pyjson::{self, Value};
use crate::sha256;
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Held while a pty is opened and while any child is spawned, so no child
/// inherits another scenario's pty before it is marked close-on-exec.
pub static SPAWN_LOCK: Mutex<()> = Mutex::new(());

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `tempfile.mkdtemp(prefix="hbp-")`, kept short for macOS socket paths.
fn mkdtemp() -> PathBuf {
    let system = std::env::temp_dir();
    let base = if system.as_os_str().len() > 24 { PathBuf::from("/tmp") } else { system };
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_";
    loop {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let seed = format!("{}-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed), nanos);
        let digest = sha256::digest(seed.as_bytes());
        let name: String = digest[..8].iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
        let path = base.join(format!("hbp-{}", name));
        if std::fs::create_dir(&path).is_ok() {
            return path;
        }
    }
}

/// A PATH entry with a `ps` that dates the panes and a silent `fc-match`.
fn fake_tools(directory: &Path, machine: &Value) -> PathBuf {
    let bin = directory.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let table: Vec<String> = machine
        .get("etimes")
        .as_dict()
        .unwrap()
        .iter()
        .map(|(pid, etime)| format!("{}) echo '{}' ;;", pid, etime.as_str().unwrap()))
        .collect();
    let scripts = [
        ("ps", format!("#!/bin/sh\nfor last; do :; done\ncase \"$last\" in {} *) exit 1 ;; esac\n", table.join(" "))),
        ("fc-match", "#!/bin/sh\nexit 1\n".to_string()),
    ];
    for (name, body) in scripts {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

pub struct Machine {
    pub tmp: PathBuf,
    pub socket_path: PathBuf,
    /// The herdr-bar root that the fake Herdr lists.
    pub plugin_root: PathBuf,
    state_dir: PathBuf,
    env: Vec<(String, String)>,
    pub server: Option<Herdr>,
}

impl Machine {
    pub fn new(machine: &Value, scenario: &Value) -> Machine {
        let tmp = mkdtemp();
        let socket_path = tmp.join("herdr.sock");
        let config_dir = tmp.join("config");
        let state_dir = tmp.join("state");
        let home = tmp.join("home");
        for path in [&config_dir, &state_dir, &home] {
            std::fs::create_dir_all(path).unwrap();
        }
        let bin = fake_tools(&tmp, machine);
        let plugin_root =
            if scenario.get("kind").as_str() == Some("watch") { linked_root(&tmp) } else { PathBuf::from(ROOT) };
        let mut this = Machine {
            tmp: tmp.clone(),
            socket_path: socket_path.clone(),
            plugin_root: plugin_root.clone(),
            state_dir: state_dir.clone(),
            env: Vec::new(),
            server: None,
        };
        if let Some(config) = scenario.get("config").as_str() {
            std::fs::write(config_dir.join("config.json"), config).unwrap();
        }
        if scenario.get("socket_up").truthy() {
            let plugins = match scenario.get("plugins") {
                Value::Null => None,
                plugins => Some(with_local_root(plugins, &plugin_root)),
            };
            let root = plugin_root.to_str().unwrap();
            this.server = Some(Herdr::start(&socket_path, scenario.get("data").clone(), root, plugins, machine));
        }
        if scenario.get("watcher_stopped").truthy() {
            std::fs::create_dir_all(this.watcher_dir()).unwrap();
            std::fs::write(this.watcher_dir().join("title-watcher.stopped"), "").unwrap();
        }
        if let Some(files) = scenario.get("files").as_dict() {
            for (relative, content) in files.iter() {
                let relative = relative.replace("{watcher}", this.watcher_dir().to_str().unwrap());
                let path = state_dir.join(relative);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, content.as_str().unwrap()).unwrap();
            }
        }
        let s = |p: &Path| p.to_str().unwrap().to_string();
        this.env = vec![
            ("PATH".into(), format!("{}:{}", s(&bin), machine.get("path").as_str().unwrap())),
            ("HOME".into(), s(&home)),
            ("TERM".into(), "xterm-256color".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("LC_ALL".into(), "C.UTF-8".into()),
            ("HERDR_SOCKET_PATH".into(), s(&socket_path)),
            ("HERDR_PLUGIN_CONFIG_DIR".into(), s(&config_dir)),
            ("HERDR_PLUGIN_STATE_DIR".into(), s(&state_dir)),
            ("HERDR_BIN_PATH".into(), "/nonexistent/herdr".into()),
        ];
        if let Some(extra) = scenario.get("env").as_dict() {
            for (key, value) in extra.iter() {
                this.env.retain(|(k, _)| k != key);
                this.env.push((key.clone(), value.as_str().unwrap_or("").to_string()));
            }
        }
        this
    }

    pub fn watcher_dir(&self) -> PathBuf {
        let key = sha256::hexdigest(self.socket_path.to_str().unwrap().as_bytes());
        self.state_dir.join("title-servers").join(&key[..24])
    }

    pub fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-bar"));
        command.args(args).env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(ROOT);
        command
    }

    /// Every file under the state directory, volatile parts masked.
    pub fn state_files(&self) -> Value {
        let watcher = self.watcher_dir().strip_prefix(&self.state_dir).unwrap().to_str().unwrap().to_string();
        let mut out = BTreeMap::new();
        let mut pending = vec![self.state_dir.clone()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                let relative = path.strip_prefix(&self.state_dir).unwrap().to_str().unwrap().replace(&watcher, "{watcher}");
                let Ok(raw) = std::fs::read(&path) else { continue };
                let mut text = String::from_utf8_lossy(&raw).into_owned();
                if entry.file_name() == "recent.json" {
                    if let Ok(mut parsed) = pyjson::loads(&text) {
                        if let Some(Value::List(entries)) = parsed.as_dict_mut().and_then(|d| d.get_mut("entries")) {
                            for entry in entries.iter_mut() {
                                if let Some(entry) = entry.as_dict_mut() {
                                    entry.insert("ts", Value::str("<ts>"));
                                }
                            }
                        }
                        text = pyjson::dumps(&parsed, false);
                    }
                }
                out.insert(relative, Value::Str(text));
            }
        }
        Value::Dict(out.into_iter().collect())
    }
}

/// A plugin root laid out as an install: the manifest, and this build at
/// `bin/herdr-bar`, both linked. The watcher stats the binary in its root to
/// notice upgrades, and the checkout has none unless the build step ran.
/// (parity.py's Python watcher found `run.py` in the checkout instead.)
fn linked_root(directory: &Path) -> PathBuf {
    let root = directory.join("plugin");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::os::unix::fs::symlink(Path::new(ROOT).join("herdr-plugin.toml"), root.join("herdr-plugin.toml")).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_herdr-bar"), root.join("bin").join("herdr-bar")).unwrap();
    std::fs::canonicalize(&root).unwrap()
}

/// parity.py named its own root as the herdr-bar plugin's; so does this run.
fn with_local_root(plugins: &Value, root: &Path) -> Value {
    let mut plugins = plugins.clone();
    if let Value::List(entries) = &mut plugins {
        for entry in entries.iter_mut() {
            if let Some(entry) = entry.as_dict_mut() {
                if entry.get("plugin_id").and_then(Value::as_str) == Some("paulmest.herdr-bar") && entry.contains_key("plugin_root") {
                    entry.insert("plugin_root", Value::str(root.to_str().unwrap()));
                }
            }
        }
    }
    plugins
}

impl Drop for Machine {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            server.close();
        }
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

// -- the plugin on a pseudo-terminal -----------------------------------------

const APPEARANCE_QUERY: &[u8] = b"\x1b]11;?";
const APPEARANCE_REPLY: &[u8] = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\";

pub struct Session {
    master: OwnedFd,
    pub child: Child,
    pub status: Option<i32>,
    pub output: Vec<u8>,
    answered: usize,
}

fn cloexec(fd: i32) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
}

impl Session {
    pub fn spawn(mut command: Command, width: u16, height: u16) -> Session {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize { ws_row: height, ws_col: width, ws_xpixel: 0, ws_ypixel: 0 };
        let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        cloexec(master);
        cloexec(slave);
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) };
        let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
        command.stdin(io(&slave)).stdout(io(&slave)).stderr(io(&slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn herdr-bar");
        drop(command);
        drop(slave);
        Session { master, child, status: None, output: Vec::new(), answered: 0 }
    }

    /// Read whatever arrives within `timeout`; false once the pty is closed.
    pub fn pump(&mut self, timeout: f64) -> bool {
        let mut poll = libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = (timeout.max(0.0) * 1000.0).ceil() as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, ms) };
        if ready <= 0 {
            return true;
        }
        let mut buffer = vec![0u8; 65536];
        let n = unsafe { libc::read(self.master.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) || error.raw_os_error() == Some(libc::EAGAIN) {
                return true;
            }
            return false; // EIO: the slave side is gone
        }
        if n == 0 {
            return false;
        }
        self.output.extend_from_slice(&buffer[..n as usize]);
        // Be the terminal: answer the background color query.
        while let Some(found) = find(&self.output[self.answered..], APPEARANCE_QUERY) {
            self.answered += found + APPEARANCE_QUERY.len();
            self.send(APPEARANCE_REPLY);
        }
        true
    }

    pub fn send(&self, data: &[u8]) {
        let mut rest = data;
        while !rest.is_empty() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
            if n <= 0 {
                return;
            }
            rest = &rest[n as usize..];
        }
    }

    /// Read until nothing comes for `quiet` seconds, or the pty closes.
    pub fn drain(&mut self, quiet: f64) {
        loop {
            let before = self.output.len();
            if !self.pump(quiet) || self.output.len() == before {
                return;
            }
        }
    }

    /// Whether the plugin has exited, collecting its raw wait status.
    pub fn reap(&mut self) -> bool {
        if self.status.is_some() {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.status = Some(status.into_raw());
                true
            }
            _ => false,
        }
    }

    pub fn wait_reaped(&mut self, timeout: f64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout);
        while std::time::Instant::now() < deadline {
            if self.reap() {
                return true;
            }
            self.pump(0.01);
        }
        self.reap()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.status.is_none() {
            unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) };
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A plain child (watcher, cli) that is killed and reaped if left running.
pub struct Guard(pub Child);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
