//! Full-text search over every pane: the `/` filter.
//!
//! The bar's own search matches names. This one matches what was said and
//! printed. Full-screen agents (Claude, Codex) keep almost nothing in
//! Herdr's scrollback, so their panes are searched through the transcript
//! the agent writes to disk; every other pane through Herdr's scrollback.
//!
//! A search reads hundreds of megabytes, so it runs on its own thread, only
//! for the `/` filter, and only once typing pauses. A newer query cancels an
//! older one between lines.

use crate::auto_title::TranscriptReader;
use crate::client::HerdrClient;
use crate::pyjson::{self, Value};
use crate::pyos;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Characters of text before and after a hit in the list row. Little before,
/// so the match sits near the start of the row's detail and the hit count
/// ahead of it stays on screen.
const ROW_BEFORE: usize = 30;
const ROW_AFTER: usize = 90;
/// Characters of text either side of a hit in the preview.
const PREVIEW_CONTEXT: usize = 280;
/// Hits kept per pane for the preview; the count still covers all of them.
const KEPT_HITS: usize = 25;
/// Scrollback rows asked of Herdr for a pane without a transcript.
const SCROLLBACK_LINES: i64 = 100_000;
const WORKERS: usize = 8;
/// A query shorter than this would match nearly everything.
pub const MIN_QUERY: usize = 2;

/// One pane to search, from the session snapshot.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub pane_id: String,
    pub tab_id: String,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub cwd: String,
    pub title: String,
}

/// The panes of a snapshot, in snapshot order.
pub fn targets(snapshot: &Value) -> Vec<Target> {
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    snapshot
        .get("panes")
        .list_or_empty()
        .iter()
        .filter_map(|pane| {
            let pane_id = text(pane.get("pane_id"));
            if pane_id.is_empty() {
                return None;
            }
            let agent = pane.get("agent").as_str().filter(|a| !a.is_empty()).map(String::from);
            let session = pane.get("agent_session");
            let session_id = if session.get("kind").as_str() == Some("id") {
                session.get("value").as_str().filter(|v| !v.is_empty()).map(String::from)
            } else {
                None
            };
            let title = pane
                .get("terminal_title_stripped")
                .as_str()
                .or_else(|| pane.get("terminal_title").as_str())
                .unwrap_or("")
                .to_string();
            Some(Target { pane_id, tab_id: text(pane.get("tab_id")), agent, session_id, cwd: text(pane.get("cwd")), title })
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// ISO 8601, or empty for scrollback, which has no clock.
    pub timestamp: String,
    /// "user", "assistant", "tool", or "output" for scrollback.
    pub role: &'static str,
    /// One line around the match, for the list row.
    pub snippet: String,
    /// More text around the match, for the preview.
    pub context: String,
}

#[derive(Clone, Debug)]
pub struct PaneHits {
    pub pane_id: String,
    pub tab_id: String,
    pub count: usize,
    /// The newest hits, oldest first.
    pub hits: Vec<Hit>,
}

impl PaneHits {
    pub fn newest(&self) -> &str {
        self.hits.last().map(|hit| hit.timestamp.as_str()).unwrap_or("")
    }
}

// -- matching ---------------------------------------------------------------

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// A case-insensitive literal needle.
#[derive(Clone, Debug)]
pub struct Needle {
    chars: Vec<char>,
    /// Lowercase ASCII bytes for the raw-line prefilter, when the needle can
    /// be found in a JSON line as written: no quotes, backslashes, controls
    /// or non-ASCII, which JSON might escape.
    raw: Option<Vec<u8>>,
}

impl Needle {
    pub fn new(query: &str) -> Needle {
        let chars: Vec<char> = query.chars().map(fold).collect();
        let raw = query
            .bytes()
            .all(|b| b.is_ascii() && !b.is_ascii_control() && b != b'"' && b != b'\\')
            .then(|| query.bytes().map(|b| b.to_ascii_lowercase()).collect());
        Needle { chars, raw }
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Whether a raw line could hold a match. Always true when unsure.
    fn may_match(&self, line: &[u8]) -> bool {
        let Some(raw) = &self.raw else { return true };
        if raw.len() > line.len() {
            return false;
        }
        let first = raw[0];
        (0..=line.len() - raw.len()).any(|at| {
            line[at].to_ascii_lowercase() == first
                && line[at..at + raw.len()].iter().zip(raw).all(|(a, b)| a.to_ascii_lowercase() == *b)
        })
    }

    /// Char ranges of every non-overlapping match in `text`.
    pub fn find_all(&self, text: &[char]) -> Vec<(usize, usize)> {
        let n = self.chars.len();
        let mut out = Vec::new();
        if n == 0 || text.len() < n {
            return out;
        }
        let mut at = 0;
        while at + n <= text.len() {
            if text[at..at + n].iter().zip(&self.chars).all(|(a, b)| fold(*a) == *b) {
                out.push((at, at + n));
                at += n;
            } else {
                at += 1;
            }
        }
        out
    }
}

/// Text on one line: runs of whitespace, newlines included, become a space.
fn flatten(chars: &[char]) -> String {
    let mut out = String::new();
    let mut space = false;
    for &c in chars {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

fn excerpt(chars: &[char], start: usize, end: usize, before: usize, after: usize) -> String {
    let from = start.saturating_sub(before);
    let to = (end + after).min(chars.len());
    let mut text = flatten(&chars[from..to]);
    if from > 0 {
        text.insert(0, '…');
    }
    if to < chars.len() {
        text.push('…');
    }
    text
}

/// Collects one pane's hits, keeping the newest few.
struct Collector<'a> {
    needle: &'a Needle,
    count: usize,
    hits: Vec<Hit>,
}

impl<'a> Collector<'a> {
    fn new(needle: &'a Needle) -> Self {
        Collector { needle, count: 0, hits: Vec::new() }
    }

    fn scan(&mut self, timestamp: &str, role: &'static str, text: &str) {
        if text.is_empty() {
            return;
        }
        let chars: Vec<char> = text.chars().collect();
        for (start, end) in self.needle.find_all(&chars) {
            self.count += 1;
            self.hits.push(Hit {
                timestamp: timestamp.to_string(),
                role,
                snippet: excerpt(&chars, start, end, ROW_BEFORE, ROW_AFTER),
                context: excerpt(&chars, start, end, PREVIEW_CONTEXT, PREVIEW_CONTEXT),
            });
            if self.hits.len() > KEPT_HITS * 2 {
                self.hits.drain(..self.hits.len() - KEPT_HITS);
            }
        }
    }

    fn finish(mut self) -> (usize, Vec<Hit>) {
        if self.hits.len() > KEPT_HITS {
            self.hits.drain(..self.hits.len() - KEPT_HITS);
        }
        (self.count, self.hits)
    }
}

// -- transcripts ------------------------------------------------------------

/// The strings inside a value, one per line: a tool's input reads as the
/// command or text it carried rather than as escaped JSON.
fn as_text(value: &Value) -> String {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Str(text) => out.push(text.clone()),
            Value::List(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Dict(map) => map.iter().for_each(|(_, item)| walk(item, out)),
            Value::Null | Value::Bool(_) => {}
            other => out.push(pyjson::py_str(other)),
        }
    }
    let mut out = Vec::new();
    walk(value, &mut out);
    out.join("\n")
}

/// The text of a Claude `tool_result` block: a string or a list of parts.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::List(parts) => {
            parts.iter().filter_map(|part| part.get("text").as_str()).collect::<Vec<_>>().join("\n")
        }
        other => as_text(other),
    }
}

/// Every searchable text of one Claude transcript line.
pub fn claude_texts(record: &Value, tools: bool, out: &mut Vec<(&'static str, String)>) {
    let role: &'static str = match record.get("type").as_str() {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return,
    };
    if record.get("isMeta").truthy() {
        return;
    }
    match record.get("message").get("content") {
        Value::Str(text) => out.push((role, text.clone())),
        Value::List(blocks) => {
            for block in blocks {
                match block.get("type").as_str() {
                    Some("text") => out.push((role, as_text(block.get("text")))),
                    Some("tool_use") if tools => out.push(("tool", as_text(block.get("input")))),
                    Some("tool_result") if tools => out.push(("tool", tool_result_text(block.get("content")))),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Every searchable text of one Codex rollout line.
pub fn codex_texts(record: &Value, tools: bool, out: &mut Vec<(&'static str, String)>) {
    if record.get("type").as_str() != Some("response_item") {
        return;
    }
    let payload = record.get("payload");
    match payload.get("type").as_str() {
        Some("message") => {
            let role: &'static str = match payload.get("role").as_str() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => return,
            };
            for part in payload.get("content").list_or_empty() {
                if matches!(part.get("type").as_str(), Some("input_text") | Some("output_text")) {
                    out.push((role, as_text(part.get("text"))));
                }
            }
        }
        Some("custom_tool_call") if tools => out.push(("tool", as_text(payload.get("input")))),
        Some("function_call") if tools => out.push(("tool", as_text(payload.get("arguments")))),
        Some("custom_tool_call_output") | Some("function_call_output") if tools => {
            out.push(("tool", as_text(payload.get("output"))))
        }
        _ => {}
    }
}

type Extract = fn(&Value, bool, &mut Vec<(&'static str, String)>);

/// Search a JSONL transcript. None when cancelled.
fn search_transcript(path: &Path, extract: Extract, needle: &Needle, cancelled: &dyn Fn() -> bool) -> Option<(usize, Vec<Hit>)> {
    let Ok(file) = File::open(path) else { return Some((0, Vec::new())) };
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut collector = Collector::new(needle);
    let mut line: Vec<u8> = Vec::new();
    let mut texts: Vec<(&'static str, String)> = Vec::new();
    let mut lines = 0usize;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        lines += 1;
        if lines.is_multiple_of(256) && cancelled() {
            return None;
        }
        if !needle.may_match(&line) {
            continue;
        }
        let Ok(record) = pyjson::loads_bytes(&line) else { continue };
        texts.clear();
        extract(&record, true, &mut texts);
        let timestamp = record.get("timestamp").as_str().unwrap_or("");
        for (role, text) in &texts {
            collector.scan(timestamp, role, text);
        }
    }
    Some(collector.finish())
}

fn codex_home() -> PathBuf {
    let home = std::env::var("CODEX_HOME").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "~/.codex".into());
    PathBuf::from(pyos::expanduser(&home))
}

/// Codex's thread names, newest last, read once per search.
#[derive(Default)]
pub struct CodexIndex {
    names: Vec<(String, String)>,
}

impl CodexIndex {
    pub fn load(home: &Path) -> CodexIndex {
        let mut names = Vec::new();
        if let Ok(file) = File::open(home.join("session_index.jsonl")) {
            for line in BufReader::new(file).lines().map_while(|l| l.ok()) {
                let Ok(row) = pyjson::loads(&line) else { continue };
                if let (Some(id), Some(name)) = (row.get("id").as_str(), row.get("thread_name").as_str()) {
                    names.push((name.to_string(), id.to_string()));
                }
            }
        }
        CodexIndex { names }
    }

    /// Thread ids named `name`, newest first.
    pub fn ids_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.names.iter().rev().filter(move |(n, _)| n == name).map(|(_, id)| id.as_str())
    }
}

/// A Codex pane's title is "<thread name> | <folder>".
pub fn codex_thread_name(title: &str) -> &str {
    match title.rfind(" | ") {
        Some(at) => title[..at].trim(),
        None => title.trim(),
    }
}

/// The rollout file of a Codex thread: sessions/YYYY/MM/DD/rollout-…-<id>.jsonl,
/// searched newest day first.
fn codex_rollout(home: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{}.jsonl", id);
    let sorted = |dir: &Path| -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> =
            std::fs::read_dir(dir).map(|it| it.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        entries.reverse();
        entries
    };
    for year in sorted(&home.join("sessions")) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                for file in sorted(&day) {
                    if file.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)) {
                        return Some(file);
                    }
                }
            }
        }
    }
    None
}

/// Char positions of every match of `needle` in `text`, for highlighting.
pub fn match_positions(needle: &Needle, text: &str) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    needle.find_all(&chars).into_iter().flat_map(|(start, end)| start..end).collect()
}

/// `text` wrapped to `width` columns between words (a word wider than the
/// line is cut), each line with the char positions of `needle`'s matches.
///
/// Matching runs on the whole text before it is cut into lines, so a match
/// that a line break splits is marked on both lines; the space the break
/// replaces is the only character no line shows.
pub fn wrap_marked(text: &str, width: isize, needle: Option<&Needle>) -> Vec<(String, Vec<usize>)> {
    use crate::textutil::char_width;
    let chars: Vec<char> = text.chars().collect();
    let mut marked = vec![false; chars.len()];
    if let Some(needle) = needle {
        for (start, end) in needle.find_all(&chars) {
            marked[start..end].iter_mut().for_each(|m| *m = true);
        }
    }
    let width = width.max(1) as usize;
    // Line spans [start, end) in chars; breaks happen at spaces, which no
    // line keeps, or mid-word when a word alone is wider than the line.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        while at < chars.len() && chars[at] == ' ' {
            at += 1;
        }
        if at >= chars.len() {
            break;
        }
        let start = at;
        let mut used = 0;
        let mut last_space: Option<usize> = None;
        let mut end = start;
        while end < chars.len() {
            let w = char_width(chars[end]);
            if used + w > width {
                break;
            }
            if chars[end] == ' ' {
                last_space = Some(end);
            }
            used += w;
            end += 1;
        }
        if end < chars.len() {
            if chars[end] == ' ' {
                // The line ends exactly at a word's end.
            } else if let Some(space) = last_space.filter(|s| *s > start) {
                end = space;
            } else if end == start {
                end = start + 1; // a char wider than the line
            }
        }
        let mut stop = end;
        while stop > start && chars[stop - 1] == ' ' {
            stop -= 1;
        }
        spans.push((start, stop));
        at = end;
    }
    spans
        .into_iter()
        .map(|(start, end)| {
            let line: String = chars[start..end].iter().collect();
            let marks = (start..end).filter(|&i| marked[i]).map(|i| i - start).collect();
            (line, marks)
        })
        .collect()
}

// -- the search -------------------------------------------------------------

struct Shared<'a> {
    needle: &'a Needle,
    claude: TranscriptReader,
    codex_home: PathBuf,
    codex_index: CodexIndex,
    generation: &'a AtomicU64,
    mine: u64,
}

impl Shared<'_> {
    fn cancelled(&self) -> bool {
        self.generation.load(Ordering::Relaxed) != self.mine
    }

    fn transcript(&self, target: &Target) -> Option<(PathBuf, Extract)> {
        match target.agent.as_deref() {
            Some("claude") => {
                let id = target.session_id.as_deref()?;
                Some((self.claude.find_path(id, &target.cwd)?, claude_texts as Extract))
            }
            Some("codex") => {
                if let Some(id) = target.session_id.as_deref() {
                    if let Some(path) = codex_rollout(&self.codex_home, id) {
                        return Some((path, codex_texts as Extract));
                    }
                }
                let name = codex_thread_name(&target.title);
                if name.is_empty() {
                    return None;
                }
                self.codex_index
                    .ids_named(name)
                    .find_map(|id| codex_rollout(&self.codex_home, id))
                    .map(|path| (path, codex_texts as Extract))
            }
            _ => None,
        }
    }

    fn search(&self, target: &Target, client: &mut Option<HerdrClient>) -> Option<PaneHits> {
        if self.cancelled() {
            return None;
        }
        let (count, hits) = match self.transcript(target) {
            Some((path, extract)) => search_transcript(&path, extract, self.needle, &|| self.cancelled())?,
            None => {
                let client = client.get_or_insert_with(HerdrClient::from_env);
                let text = client.read_pane(&target.pane_id, SCROLLBACK_LINES, "recent-unwrapped").unwrap_or_default();
                if self.cancelled() {
                    return None;
                }
                let mut collector = Collector::new(self.needle);
                for line in text.lines() {
                    collector.scan("", "output", line);
                }
                collector.finish()
            }
        };
        Some(PaneHits { pane_id: target.pane_id.clone(), tab_id: target.tab_id.clone(), count, hits })
    }
}

/// Search every target for `query`, newest hits first. None when a newer
/// search took over (`generation` moved past `mine`) before this one ended.
pub fn search(targets: &[Target], query: &str, generation: &AtomicU64, mine: u64) -> Option<Vec<PaneHits>> {
    let needle = Needle::new(query);
    if needle.is_empty() {
        return Some(Vec::new());
    }
    let home = codex_home();
    let wants_codex = targets.iter().any(|t| t.agent.as_deref() == Some("codex") && t.session_id.is_none());
    let shared = Shared {
        needle: &needle,
        claude: TranscriptReader::new(None),
        codex_index: if wants_codex { CodexIndex::load(&home) } else { CodexIndex::default() },
        codex_home: home,
        generation,
        mine,
    };
    let next = AtomicUsize::new(0);
    let workers = WORKERS.min(targets.len()).max(1);
    let found: Vec<Option<Vec<PaneHits>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut client: Option<HerdrClient> = None;
                    let mut out = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(target) = targets.get(at) else { break };
                        match shared.search(target, &mut client) {
                            Some(hits) if hits.count > 0 => out.push(hits),
                            Some(_) => {}
                            None => return None,
                        }
                    }
                    Some(out)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(None)).collect()
    });
    let mut results = Vec::new();
    for part in found {
        results.extend(part?);
    }
    if shared.cancelled() {
        return None;
    }
    // Newest hit first; scrollback, which has no clock, after the transcripts.
    results.sort_by(|a, b| b.newest().cmp(a.newest()).then_with(|| b.count.cmp(&a.count)));
    Some(results)
}

/// What a finished search sends back: its generation, query and hits
/// (None when cancelled).
type Finished = (u64, String, Option<Vec<PaneHits>>);

/// A search on its own thread, so the bar keeps drawing and taking keys.
pub struct Background {
    generation: Arc<AtomicU64>,
    receiver: Option<std::sync::mpsc::Receiver<Finished>>,
    running: Option<(u64, String)>,
}

impl Default for Background {
    fn default() -> Self {
        Background { generation: Arc::new(AtomicU64::new(0)), receiver: None, running: None }
    }
}

impl Background {
    /// Start searching, cancelling any search still running.
    pub fn start(&mut self, targets: Vec<Target>, query: String) {
        let mine = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = std::sync::mpsc::channel();
        let generation = Arc::clone(&self.generation);
        let label = query.clone();
        let spawned = std::thread::Builder::new().name("fulltext".into()).spawn(move || {
            let found = search(&targets, &query, &generation, mine);
            let _ = sender.send((mine, query, found));
        });
        if spawned.is_ok() {
            self.receiver = Some(receiver);
            self.running = Some((mine, label));
        }
    }

    /// Stop caring about the running search, if any.
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.receiver = None;
        self.running = None;
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The finished search's query and hits, once, if it is the latest.
    pub fn poll(&mut self) -> Option<(String, Vec<PaneHits>)> {
        let receiver = self.receiver.as_ref()?;
        let (mine, query, found) = match receiver.try_recv() {
            Ok(message) => message,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                self.running = None;
                return None;
            }
        };
        if self.running.as_ref().map(|(id, _)| *id) != Some(mine) {
            return None;
        }
        self.receiver = None;
        self.running = None;
        found.map(|hits| (query, hits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn needles_match_without_case() {
        let needle = Needle::new("ENG-98");
        assert_eq!(needle.find_all(&chars("fix eng-9813 and Eng-9814")), vec![(4, 10), (17, 23)]);
    }

    #[test]
    fn needles_match_non_ascii_without_case() {
        let needle = Needle::new("ÉCOLE");
        assert_eq!(needle.find_all(&chars("une école")), vec![(4, 9)]);
        assert!(needle.raw.is_none(), "non-ASCII needles skip the raw prefilter");
    }

    #[test]
    fn the_prefilter_skips_lines_without_the_needle() {
        let needle = Needle::new("Rebase");
        assert!(needle.may_match(br#"{"text":"please REBASE it"}"#));
        assert!(!needle.may_match(br#"{"text":"merge it"}"#));
        // Quotes are escaped inside JSON strings, so such needles check every line.
        assert!(Needle::new("say \"hi\"").may_match(b"anything"));
    }

    #[test]
    fn snippets_sit_on_one_line_with_ellipses() {
        let text = chars(&format!("{}needle\n\nafter", "x".repeat(100)));
        let snippet = excerpt(&text, 100, 106, 5, 5);
        assert_eq!(snippet, "…xxxxxneedle aft…");
    }

    #[test]
    fn claude_lines_yield_prompts_replies_and_tools() {
        let mut out = Vec::new();
        let user = pyjson::loads(r#"{"type":"user","message":{"content":"hello there"}}"#).unwrap();
        claude_texts(&user, true, &mut out);
        let assistant = pyjson::loads(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","input":{"command":"ls"}}]}}"#,
        )
        .unwrap();
        claude_texts(&assistant, true, &mut out);
        let result = pyjson::loads(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"a.txt"}]}]}}"#,
        )
        .unwrap();
        claude_texts(&result, true, &mut out);
        let meta = pyjson::loads(r#"{"type":"user","isMeta":true,"message":{"content":"caveat"}}"#).unwrap();
        claude_texts(&meta, true, &mut out);
        let other = pyjson::loads(r#"{"type":"system","message":{"content":"nope"}}"#).unwrap();
        claude_texts(&other, true, &mut out);
        assert_eq!(
            out,
            vec![
                ("user", "hello there".to_string()),
                ("assistant", "hi".to_string()),
                ("tool", "ls".to_string()),
                ("tool", "a.txt".to_string()),
            ]
        );
    }

    #[test]
    fn codex_lines_yield_messages_and_tools() {
        let mut out = Vec::new();
        for line in [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"find it"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"found"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","input":"rg x"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","output":"done"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
        ] {
            codex_texts(&pyjson::loads(line).unwrap(), true, &mut out);
        }
        assert_eq!(
            out,
            vec![
                ("user", "find it".to_string()),
                ("assistant", "found".to_string()),
                ("tool", "rg x".to_string()),
                ("tool", "done".to_string()),
            ]
        );
    }

    fn wrapped(text: &str, width: isize, query: &str) -> Vec<(String, Vec<usize>)> {
        wrap_marked(text, width, Some(&Needle::new(query)))
    }

    #[test]
    fn wrapping_marks_matches_on_their_line() {
        assert_eq!(
            wrapped("one two needle three", 12, "NEEDLE"),
            vec![("one two".to_string(), vec![]), ("needle three".to_string(), vec![0, 1, 2, 3, 4, 5])]
        );
    }

    #[test]
    fn a_match_split_by_a_line_break_is_marked_on_both_lines() {
        // "pane list" breaks after "pane": both halves light up.
        let lines = wrapped("run herdr pane list now", 14, "pane list");
        assert_eq!(
            lines,
            vec![("run herdr pane".to_string(), vec![10, 11, 12, 13]), ("list now".to_string(), vec![0, 1, 2, 3]),]
        );
    }

    #[test]
    fn a_match_inside_a_cut_word_is_marked_across_the_cut() {
        let lines = wrapped("abcdefghij", 4, "def");
        assert_eq!(
            lines,
            vec![
                ("abcd".to_string(), vec![3]),
                ("efgh".to_string(), vec![0, 1]),
                ("ij".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn wrapping_without_a_needle_marks_nothing_and_keeps_every_word() {
        let lines = wrap_marked("aa bb cc dd", 5, None);
        assert_eq!(lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["aa bb", "cc dd"]);
        assert!(lines.iter().all(|(_, m)| m.is_empty()));
    }

    #[test]
    fn match_positions_are_char_positions() {
        assert_eq!(match_positions(&Needle::new("É"), "café É"), vec![3, 5]);
    }

    #[test]
    fn codex_titles_name_the_thread() {
        assert_eq!(codex_thread_name("Assess herdr pane timelines | eluve-nx"), "Assess herdr pane timelines");
        assert_eq!(codex_thread_name("a | b | repo"), "a | b");
        assert_eq!(codex_thread_name("plain"), "plain");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-bar-fulltext-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn transcripts_count_every_hit_and_keep_the_newest() {
        let dir = temp_dir("count");
        let path = dir.join("s.jsonl");
        let mut body = String::new();
        for i in 0..(KEPT_HITS + 5) {
            body.push_str(&format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-01-01T00:00:{:02}Z\",\"message\":{{\"content\":\"needle {}\"}}}}\n",
                i, i
            ));
        }
        body.push_str("not json\n");
        std::fs::write(&path, body).unwrap();
        let (count, hits) = search_transcript(&path, claude_texts, &Needle::new("NEEDLE"), &|| false).unwrap();
        assert_eq!(count, KEPT_HITS + 5);
        assert_eq!(hits.len(), KEPT_HITS);
        assert_eq!(hits.last().unwrap().snippet, format!("needle {}", KEPT_HITS + 4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_threads_resolve_by_name_to_their_rollout() {
        let home = temp_dir("codex");
        let day = home.join("sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            "{\"id\":\"old-id\",\"thread_name\":\"Fix it\"}\n{\"id\":\"new-id\",\"thread_name\":\"Fix it\"}\n",
        )
        .unwrap();
        let rollout = day.join("rollout-2026-11-06T01-00-00-new-id.jsonl");
        std::fs::write(&rollout, "").unwrap();
        let index = CodexIndex::load(&home);
        let found = index.ids_named("Fix it").find_map(|id| codex_rollout(&home, id));
        assert_eq!(found, Some(rollout));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn targets_come_from_snapshot_panes() {
        let snapshot = pyjson::loads(
            r#"{"panes":[
                {"pane_id":"w1:p1","tab_id":"w1:t1","agent":"claude","cwd":"/r","terminal_title_stripped":"T",
                 "agent_session":{"kind":"id","value":"abc"}},
                {"pane_id":"w1:p2","tab_id":"w1:t1","cwd":"/r"},
                {"tab_id":"w1:t2"}
            ]}"#,
        )
        .unwrap();
        let found = targets(&snapshot);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].agent.as_deref(), Some("claude"));
        assert_eq!(found[0].session_id.as_deref(), Some("abc"));
        assert_eq!(found[0].title, "T");
        assert_eq!(found[1].agent, None);
    }

    #[test]
    fn a_newer_search_cancels_an_older_one() {
        let generation = AtomicU64::new(2);
        let targets = vec![Target { pane_id: "p".into(), agent: Some("claude".into()), ..Target::default() }];
        assert!(search(&targets, "x", &generation, 1).is_none());
        assert_eq!(search(&targets, "", &generation, 2).map(|r| r.len()), Some(0));
    }
}
