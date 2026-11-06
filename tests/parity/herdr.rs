//! A fake Herdr on a Unix socket that records requests (parity.py's
//! RecordingHerdr over bench_process.py's FakeHerdr).

use crate::pyjson::{self, Map, Value};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Requests that only read; compared as a set, not a sequence.
pub const READS: [&str; 5] = ["session.snapshot", "pane.read", "pane.process_info", "plugin.list", "events.subscribe"];

pub struct State {
    pub data: Value,
    log: Vec<(String, Value)>,
}

struct Shared {
    state: Mutex<State>,
    closing: AtomicBool,
    /// f64 bits of the event interval in seconds; 0 is None.
    interval: AtomicU64,
    subscribers: Mutex<Vec<UnixStream>>,
    plugin_root: String,
    plugins: Option<Value>,
    preview: String,
    pane_pids: Value,
}

pub struct Herdr {
    path: PathBuf,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
    announce: Option<JoinHandle<()>>,
}

fn encode(result: Value) -> Vec<u8> {
    let message: Map = [("id".to_string(), Value::str("bar")), ("result".to_string(), result)].into_iter().collect();
    let mut out = pyjson::dumps(&Value::Dict(message), false).into_bytes();
    out.push(b'\n');
    out
}

fn obj(key: &str, value: Value) -> Value {
    let mut map = Map::new();
    map.insert(key, value);
    Value::Dict(map)
}

impl Herdr {
    pub fn start(path: &Path, data: Value, plugin_root: &str, plugins: Option<Value>, machine: &Value) -> Herdr {
        let listener = UnixListener::bind(path).unwrap_or_else(|e| panic!("bind {}: {}", path.display(), e));
        let shared = Arc::new(Shared {
            state: Mutex::new(State { data, log: Vec::new() }),
            closing: AtomicBool::new(false),
            interval: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
            plugin_root: plugin_root.to_string(),
            plugins,
            preview: machine.get("preview_text").as_str().unwrap_or("").to_string(),
            pane_pids: machine.get("pane_pids").clone(),
        });
        let accept = {
            let shared = shared.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if shared.closing.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(conn) = conn else { continue };
                    let shared = shared.clone();
                    std::thread::spawn(move || serve(&shared, conn));
                }
            })
        };
        let announce = {
            let shared = shared.clone();
            std::thread::spawn(move || announce(&shared))
        };
        Herdr { path: path.to_path_buf(), shared, accept: Some(accept), announce: Some(announce) }
    }

    pub fn set_event_interval(&self, seconds: Option<f64>) {
        self.shared.interval.store(seconds.map(f64::to_bits).unwrap_or(0), Ordering::SeqCst);
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.shared.state.lock().unwrap())
    }

    pub fn changes(&self) -> Vec<Value> {
        self.with_state(|s| {
            s.log
                .iter()
                .filter(|(m, _)| !READS.contains(&m.as_str()))
                .map(|(m, p)| Value::List(vec![Value::str(m.clone()), p.clone()]))
                .collect()
        })
    }

    pub fn change_count(&self) -> usize {
        self.with_state(|s| s.log.iter().filter(|(m, _)| !READS.contains(&m.as_str())).count())
    }

    /// Distinct pane reads, sorted by their `json.dumps(sort_keys=True)`.
    pub fn reads(&self) -> Vec<Value> {
        let mut seen: Vec<String> = self.with_state(|s| {
            s.log
                .iter()
                .filter(|(m, _)| m == "pane.read" || m == "pane.process_info")
                .map(|(m, p)| pyjson::dumps(&Value::List(vec![Value::str(m.clone()), p.clone()]), true))
                .collect()
        });
        seen.sort();
        seen.dedup();
        seen.iter().map(|e| pyjson::loads(e).unwrap()).collect()
    }

    pub fn close(&mut self) {
        if self.shared.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = UnixStream::connect(&self.path);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        for conn in self.shared.subscribers.lock().unwrap().drain(..) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        if let Some(handle) = self.announce.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        self.close();
    }
}

fn serve(shared: &Shared, mut conn: UnixStream) {
    let mut buffered = Vec::new();
    let mut chunk = vec![0u8; 65536];
    while !buffered.contains(&b'\n') {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buffered.extend_from_slice(&chunk[..n]),
        }
    }
    let line = &buffered[..buffered.iter().position(|b| *b == b'\n').unwrap()];
    let Ok(request) = pyjson::loads_bytes(line) else { return };
    let method = request.get("method").as_str().unwrap_or("").to_string();
    let params = request.get("params");
    let params = if params.truthy() { params.clone() } else { Value::Dict(Map::new()) };
    let mut state = shared.state.lock().unwrap();
    state.log.push((method.clone(), params.clone()));
    if method == "events.subscribe" {
        drop(state);
        if conn.write_all(&encode(Value::Dict(Map::new()))).is_ok() {
            if let Ok(clone) = conn.try_clone() {
                let _ = clone.set_write_timeout(Some(Duration::from_secs(1)));
                shared.subscribers.lock().unwrap().push(clone);
            }
        }
        return;
    }
    let answer = answer(shared, &mut state.data, &method, &params);
    drop(state);
    let _ = conn.write_all(&answer);
}

fn answer(shared: &Shared, data: &mut Value, method: &str, params: &Value) -> Vec<u8> {
    match method {
        "session.snapshot" => encode(obj("snapshot", data.clone())),
        "pane.read" => encode(obj("read", obj("text", Value::str(shared.preview.clone())))),
        "pane.process_info" => {
            let pid = params.get("pane_id").as_str().map(|p| shared.pane_pids.get(p).clone()).unwrap_or(Value::Null);
            let info = if pid.truthy() { obj("shell_pid", pid) } else { Value::Dict(Map::new()) };
            encode(obj("process_info", info))
        }
        "plugin.list" => {
            let plugins = shared.plugins.clone().unwrap_or_else(|| {
                let entry: Map = [
                    ("plugin_id".to_string(), Value::str("paulmest.herdr-bar")),
                    ("enabled".to_string(), Value::Bool(true)),
                    ("plugin_root".to_string(), Value::str(shared.plugin_root.clone())),
                ]
                .into_iter()
                .collect();
                Value::List(vec![Value::Dict(entry)])
            });
            encode(obj("plugins", plugins))
        }
        "tab.rename" => {
            if let Some(Value::List(tabs)) = data.as_dict_mut().and_then(|d| d.get_mut("tabs")) {
                for tab in tabs.iter_mut() {
                    if pyjson::py_eq(tab.get("tab_id"), params.get("tab_id")) {
                        if let Some(tab) = tab.as_dict_mut() {
                            tab.insert("label", params.get("label").clone());
                        }
                    }
                }
            }
            encode(Value::Dict(Map::new()))
        }
        "tab.close" => {
            let tab_id = params.get("tab_id").clone();
            if let Some(map) = data.as_dict_mut() {
                for key in ["tabs", "agents", "panes"] {
                    let kept: Vec<Value> = map
                        .get(key)
                        .map(|v| v.list_or_empty())
                        .unwrap_or(&[])
                        .iter()
                        .filter(|entry| !pyjson::py_eq(entry.get("tab_id"), &tab_id))
                        .cloned()
                        .collect();
                    map.insert(key, Value::List(kept));
                }
            }
            encode(Value::Dict(Map::new()))
        }
        _ => encode(Value::Dict(Map::new())),
    }
}

fn announce(shared: &Shared) {
    let mut sequence = 0usize;
    while !shared.closing.load(Ordering::SeqCst) {
        let bits = shared.interval.load(Ordering::SeqCst);
        let interval = if bits == 0 { None } else { Some(f64::from_bits(bits)) };
        std::thread::sleep(Duration::from_secs_f64(interval.unwrap_or(0.05)));
        if interval.is_none() {
            continue;
        }
        sequence += 1;
        let pane = {
            let state = shared.state.lock().unwrap();
            let agents = state.data.get("agents");
            let agents: Vec<Value> = if agents.truthy() && !agents.list_or_empty().is_empty() {
                agents.list_or_empty().to_vec()
            } else {
                vec![Value::Dict(Map::new())]
            };
            match agents[sequence % agents.len()].as_dict().and_then(|a| a.get("pane_id")) {
                Some(pane) => pane.clone(),
                None => Value::str("w1:p2"),
            }
        };
        let message: Map =
            [("event".to_string(), Value::str("pane.agent_status_changed")), ("data".to_string(), obj("pane_id", pane))]
                .into_iter()
                .collect();
        let mut line = pyjson::dumps(&Value::Dict(message), false).into_bytes();
        line.push(b'\n');
        shared.subscribers.lock().unwrap().retain_mut(|conn| conn.write_all(&line).is_ok());
    }
}
