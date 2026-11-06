//! Give enabled naming plugins priority over all automatic tab writes.

use crate::client::{HerdrClient, HerdrError, Result};
use crate::config::Config;
use crate::pyjson::{self, Map, Value};
use crate::pystr;

/// IDs from the plugins' published manifests. Metadata matching also covers
/// forks; custom IDs can be listed in tab_renaming_plugins.
pub const KNOWN_RENAMERS: &[&str] = &[
    "herdr.auto-title",
    "aarsh21.tab-title",
    "herdr-tab-title",
    "zhangzujian.auto-session-title",
    "herdr-automatic-rename",
];

fn is_word(c: char) -> bool {
    c == '_' || pystr::is_alnum(c)
}

fn boundary(text: &[char], at: usize) -> bool {
    let before = at > 0 && is_word(text[at - 1]);
    let after = at < text.len() && is_word(text[at]);
    before != after
}

fn literal(text: &[char], at: usize, word: &str) -> Option<usize> {
    let mut index = at;
    for c in word.chars() {
        if index >= text.len() || text[index] != c {
            return None;
        }
        index += 1;
    }
    Some(index)
}

/// `\bauto(?:matic)? (?:session )?(?:title|rename|name)\b`
fn automatic_title(text: &[char], start: usize) -> bool {
    let Some(after_auto) = literal(text, start, "auto") else { return false };
    let autos = [literal(text, after_auto, "matic"), Some(after_auto)];
    for at in autos.into_iter().flatten() {
        let Some(at) = literal(text, at, " ") else { continue };
        for at in [literal(text, at, "session "), Some(at)].into_iter().flatten() {
            for word in ["title", "rename", "name"] {
                if let Some(end) = literal(text, at, word) {
                    if boundary(text, end) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn word_run(text: &[char], mut at: usize) -> usize {
    while at < text.len() && is_word(text[at]) {
        at += 1;
    }
    at
}

/// `\btabs? (?:titles?|names?|renam\w*|naming)\b`
fn tab_names(text: &[char], start: usize) -> bool {
    let Some(after_tab) = literal(text, start, "tab") else { return false };
    for at in [literal(text, after_tab, "s"), Some(after_tab)].into_iter().flatten() {
        let Some(at) = literal(text, at, " ") else { continue };
        let mut ends: Vec<usize> = Vec::new();
        for word in ["title", "name"] {
            if let Some(end) = literal(text, at, word) {
                ends.push(end);
                if let Some(end) = literal(text, end, "s") {
                    ends.push(end);
                }
            }
        }
        if let Some(end) = literal(text, at, "renam") {
            ends.push(word_run(text, end));
        }
        if let Some(end) = literal(text, at, "naming") {
            ends.push(end);
        }
        if ends.into_iter().any(|end| boundary(text, end)) {
            return true;
        }
    }
    false
}

/// `\brenam\w*\b.{0,40}\btabs?\b`
fn renames_tabs(text: &[char], start: usize) -> bool {
    let Some(after) = literal(text, start, "renam") else { return false };
    // Only the whole word run can end at a word boundary.
    let end = word_run(text, after);
    if !boundary(text, end) {
        return false;
    }
    for gap in 0..=40 {
        let at = end + gap;
        if at > text.len() {
            break;
        }
        if gap > 0 && text[at - 1] == '\n' {
            break;
        }
        if !boundary(text, at) {
            continue;
        }
        if let Some(after_tab) = literal(text, at, "tab") {
            for stop in [literal(text, after_tab, "s"), Some(after_tab)].into_iter().flatten() {
                if boundary(text, stop) {
                    return true;
                }
            }
        }
    }
    false
}

pub fn naming_search(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..=chars.len()).any(|start| {
        boundary(&chars, start)
            && (automatic_title(&chars, start) || tab_names(&chars, start) || renames_tabs(&chars, start))
    })
}

/// `re.sub(r"[._-]+", " ", text)`
fn squash_separators(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c == '.' || c == '_' || c == '-' {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

pub fn installed_plugins(client: &mut HerdrClient) -> Result<Vec<Map>> {
    let mut result = client.call("plugin.list", Value::Dict(Map::new()), &["plugin", "list"])?;
    let error = || HerdrError::new("plugin.list did not return a plugin list");
    let Some(Value::List(plugins)) = result.remove("plugins") else { return Err(error()) };
    let mut out = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        match plugin {
            Value::Dict(map) => out.push(map),
            _ => return Err(error()),
        }
    }
    Ok(out)
}

pub fn check_plugins(plugins: &[Map], extra: &[String]) -> Result<()> {
    let mut conflicts: Vec<String> = Vec::new();
    for plugin in plugins {
        let plugin_id = plugin.get("plugin_id").unwrap_or(&pyjson::NULL);
        let enabled = plugin.get("enabled").map(|v| v.truthy()).unwrap_or(false);
        if !enabled || plugin_id.as_str() == Some("paulmest.herdr-bar") {
            continue;
        }
        let source = match plugin.get("source") {
            Some(Value::Dict(source)) => source.get("repo").cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let values = [plugin_id.clone(), plugin.get("name").cloned().unwrap_or(Value::Null), plugin.get("description").cloned().unwrap_or(Value::Null), source];
        let metadata: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
        let metadata = squash_separators(&pystr::casefold(&metadata.join(" ")));
        let known = plugin_id.as_str().map(|id| KNOWN_RENAMERS.contains(&id) || extra.iter().any(|e| e == id)).unwrap_or(false);
        if known || naming_search(&metadata) {
            let name = plugin.get("name").unwrap_or(&pyjson::NULL);
            let label = if plugin_id.truthy() {
                pyjson::py_str(plugin_id)
            } else if name.truthy() {
                pyjson::py_str(name)
            } else {
                "unknown plugin".to_string()
            };
            conflicts.push(label);
        }
    }
    if !conflicts.is_empty() {
        return Err(HerdrError::renaming(format!(
            "automatic tab updates disabled: enabled naming plugin(s): {}",
            conflicts.join(", ")
        )));
    }
    Ok(())
}

/// Guard background writes, leaving explicit user renames on the raw client.
pub struct AutomaticTabClient {
    pub client: HerdrClient,
    pub conflict: String,
}

impl AutomaticTabClient {
    pub fn new(client: HerdrClient) -> AutomaticTabClient {
        AutomaticTabClient { client, conflict: String::new() }
    }

    pub fn snapshot(&mut self) -> Result<Value> {
        self.client.snapshot()
    }

    pub fn rename_tab(&mut self, tab_id: &str, label: &str) -> Result<()> {
        if !self.conflict.is_empty() {
            return Err(HerdrError::renaming(self.conflict.clone()));
        }
        let plugins = installed_plugins(&mut self.client)?;
        if let Err(error) = check_plugins(&plugins, &Config::load().tab_renaming_plugins) {
            self.conflict = error.message.clone();
            return Err(error);
        }
        self.client.rename_tab(tab_id, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_patterns() {
        for text in [
            "auto title",
            "automatic rename",
            "auto session name for x",
            "tab titles",
            "tabs renaming",
            "renames the current tab",
            "rename herdr tabs",
            "tab naming",
        ] {
            assert!(naming_search(text), "{}", text);
        }
        for text in ["autotitle", "tabular names", "rename\ntab", "auto titles", "stab names", "bar"] {
            assert!(!naming_search(text), "{}", text);
        }
    }

    // -- ported from tests/test_renaming_guard.py -------------------------------

    fn plugin(json: &str) -> Map {
        pyjson::loads(json).unwrap().as_dict().unwrap().clone()
    }

    fn is_detected(result: Result<()>) -> bool {
        matches!(result, Err(HerdrError { kind: crate::client::Kind::Renaming, .. }))
    }

    #[test]
    fn known_enabled_renamers_block_and_disabled_ones_do_not() {
        for id in KNOWN_RENAMERS {
            let on = plugin(&format!(r#"{{"plugin_id": "{}", "enabled": true}}"#, id));
            assert!(is_detected(check_plugins(&[on], &[])), "{}", id);
            let off = plugin(&format!(r#"{{"plugin_id": "{}", "enabled": false}}"#, id));
            assert!(check_plugins(&[off], &[]).is_ok(), "{}", id);
        }
    }

    #[test]
    fn forks_and_new_plugins_are_detected_from_metadata() {
        for fields in [
            r#""name": "Smart tab renaming""#,
            r#""description": "Automatically rename tabs from the current task""#,
            r#""source": {"repo": "herdr-auto-title"}"#,
        ] {
            let custom = plugin(&format!(r#"{{"plugin_id": "custom.plugin", "enabled": true, {}}}"#, fields));
            assert!(is_detected(check_plugins(&[custom], &[])), "{}", fields);
        }
    }

    #[test]
    fn self_and_unrelated_plugins_do_not_block() {
        let plugins = [
            plugin(r#"{"plugin_id": "paulmest.herdr-bar", "enabled": true, "description": "Automatic tab titles"}"#),
            plugin(r#"{"plugin_id": "herdr-newtab-plus", "enabled": true, "description": "Pick a folder and open a tab"}"#),
            plugin(r#"{"plugin_id": "herdr-grid", "enabled": true, "name": "Agent Grid"}"#),
        ];
        assert!(check_plugins(&plugins, &[]).is_ok());
    }

    #[test]
    fn custom_ids_can_be_declared() {
        let private = plugin(r#"{"plugin_id": "private.tools", "enabled": true}"#);
        assert!(is_detected(check_plugins(&[private], &["private.tools".to_string()])));
    }

    #[cfg(unix)]
    mod with_server {
        use super::*;
        use crate::tab_status::tests::{config, pair, quiet_env, session, FakeHerdr};
        use crate::tab_status::TabStatus;
        use crate::testutil::{env_lock, EnvVars, TempDir};

        fn plugins(server: &FakeHerdr, reply: &str) {
            server.state().plugin_reply = pyjson::loads(reply).unwrap();
        }

        #[test]
        fn plugin_enabled_between_writes_blocks_the_next_write() {
            let _env = env_lock();
            let _vars = quiet_env();
            let server = FakeHerdr::new(session("working", "2"));
            let mut writer = server.writer();
            writer.rename_tab("w1:t1", "First").unwrap();
            plugins(&server, r#"{"result": {"plugins": [{"plugin_id": "herdr.auto-title", "enabled": true}]}}"#);
            assert!(is_detected(writer.rename_tab("w1:t1", "Second")));
            plugins(&server, r#"{"result": {"plugins": []}}"#);
            assert!(is_detected(writer.rename_tab("w1:t2", "Third")), "the conflict sticks");
            assert_eq!(server.renames(), pair("w1:t1", "First"));
        }

        #[test]
        fn unavailable_or_invalid_registry_never_allows_a_write() {
            let _env = env_lock();
            let _vars = quiet_env();
            for reply in [r#"{"result": {}}"#, r#"{"result": {"plugins": [null]}}"#, r#"{"error": {"message": "offline"}}"#] {
                let server = FakeHerdr::new(session("working", "2"));
                plugins(&server, reply);
                assert!(server.writer().rename_tab("w1:t1", "Changed").is_err(), "{}", reply);
                assert_eq!(server.renames(), vec![], "{}", reply);
            }
        }

        #[test]
        fn custom_config_blocks_writes() {
            let _env = env_lock();
            let _vars = quiet_env();
            let dir = TempDir::new();
            std::fs::write(dir.path().join("config.json"), r#"{"tab_renaming_plugins": ["private.tools"]}"#).unwrap();
            let mut vars = EnvVars::new();
            vars.set("HERDR_PLUGIN_CONFIG_DIR", dir.path());
            let server = FakeHerdr::new(session("working", "2"));
            plugins(&server, r#"{"result": {"plugins": [{"plugin_id": "private.tools", "enabled": true}]}}"#);
            assert!(is_detected(server.writer().rename_tab("w1:t1", "Changed")));
            assert_eq!(server.renames(), vec![]);
        }

        #[test]
        fn competing_plugin_prevents_cleanup_writes_too() {
            let _env = env_lock();
            let _vars = quiet_env();
            let dir = TempDir::new();
            let server = FakeHerdr::new(session("working", "Fix login"));
            let mut tabs = TabStatus::new(server.writer(), Some(dir.path().to_path_buf()));
            tabs.configure(&config(r#"{"tab_status": true, "agent_icons": "none"}"#));
            let snapshot = tabs.snapshot().unwrap();
            tabs.apply(&snapshot);
            plugins(&server, r#"{"result": {"plugins": [{"plugin_id": "herdr.auto-title", "enabled": true}]}}"#);
            tabs.clear();
            assert_eq!(server.renames(), pair("w1:t2", "◐ Fix login"));
            assert_eq!(server.label("w1:t2"), "◐ Fix login");
        }
    }
}
