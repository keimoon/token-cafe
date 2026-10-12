//! Claude Code: ~/.claude/sessions/<pid>.json, one per interactive session.
//! status busy -> work, idle -> idle, waiting -> wait (waitingFor
//! "permission prompt" -> "!", anything else -> "?").

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::{Found, basename, process, truthy};
use crate::scene::{Kind, State, Why};
use crate::usage::claude_dir;

fn millis(d: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| d.get(*k).filter(|v| truthy(v)).and_then(Value::as_f64))
}

#[derive(Default)]
pub struct Claude {
    desktop: DesktopMetadata,
}

impl Claude {
    pub fn sessions(&mut self, now: f64) -> Vec<Found> {
        let desktop = if cfg!(target_os = "macos") {
            self.desktop.refresh(&crate::usage::home().join("Library/Application Support/Claude/claude-code-sessions"))
        } else {
            HashMap::new()
        };
        sessions(now, &desktop)
    }
}

#[derive(Clone)]
struct DesktopSession {
    desktop_id: Option<String>,
    id: String,
    title: String,
    archived: bool,
}

fn desktop_session(raw: &[u8]) -> Option<DesktopSession> {
    let d: Value = serde_json::from_slice(raw).ok()?;
    let id = d.get("cliSessionId")?.as_str()?.to_owned();
    if id.is_empty() {
        return None;
    }
    Some(DesktopSession {
        desktop_id: d.get("sessionId").and_then(Value::as_str).map(str::to_owned),
        id,
        title: d.get("title").and_then(Value::as_str).unwrap_or("").to_owned(),
        archived: d.get("isArchived").and_then(Value::as_bool).unwrap_or(false),
    })
}

#[derive(Default)]
struct DesktopMetadata {
    last: Option<Instant>,
    files: HashMap<PathBuf, (SystemTime, u64, Option<DesktopSession>)>,
}

impl DesktopMetadata {
    fn refresh(&mut self, root: &Path) -> HashMap<String, DesktopSession> {
        if self.last.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            let mut paths = Vec::new();
            metadata_files(root, 3, &mut paths);
            let mut fresh = HashMap::new();
            for path in paths {
                let Ok(meta) = fs::metadata(&path) else { continue };
                let Ok(modified) = meta.modified() else { continue };
                let len = meta.len();
                let entry = match self.files.remove(&path) {
                    Some(entry) if entry.0 == modified && entry.1 == len => entry,
                    _ => (modified, len, fs::read(&path).ok().and_then(|raw| desktop_session(&raw))),
                };
                fresh.insert(path, entry);
            }
            self.files = fresh;
            self.last = Some(Instant::now());
        }
        self.files.values().filter_map(|(_, _, session)| session.clone()).map(|s| (s.id.clone(), s)).collect()
    }
}

fn metadata_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && depth > 0 {
            metadata_files(&entry.path(), depth - 1, out);
        } else if kind.is_file() && entry.path().extension().is_some_and(|ext| ext == "json") {
            out.push(entry.path());
        }
    }
}

// Desktop records are saved history, not proof that a session is running.
// Only enrich sessions whose process identity passed the normal live check.
fn sessions(now: f64, desktop: &HashMap<String, DesktopSession>) -> Vec<Found> {
    sessions_at(now, &claude_dir().join("sessions"), desktop, process::matches_session)
}

fn sessions_at(
    now: f64,
    directory: &Path,
    desktop: &HashMap<String, DesktopSession>,
    alive: impl Fn(u32, &Value) -> bool,
) -> Vec<Found> {
    let mut out: HashMap<String, Found> = HashMap::new();
    let Ok(rd) = fs::read_dir(directory) else { return Vec::new() };
    for e in rd.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let Ok(raw) = fs::read(&path) else { continue };
        let Ok(d) = serde_json::from_slice::<Value>(&raw) else { continue };
        match d.get("kind") {
            None => {}
            Some(Value::String(k)) if k == "interactive" => {}
            _ => continue,
        }
        let Some(pid) = d.get("pid").filter(|p| truthy(p)).and_then(Value::as_u64) else { continue };
        let Ok(pid) = u32::try_from(pid) else { continue };
        if !alive(pid, &d) {
            continue; // dead process or a reused pid
        }
        let metadata = d.get("sessionId").and_then(Value::as_str).and_then(|id| desktop.get(id));
        if metadata.is_some_and(|s| s.archived) {
            continue;
        }
        let status = d.get("status").and_then(Value::as_str);
        let state = match status {
            Some("busy") => State::Work,
            Some("waiting") => State::Wait,
            _ => State::Idle,
        };
        let why = if state != State::Wait {
            Why::None
        } else {
            let wf =
                d.get("waitingFor").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("permission prompt");
            if wf == "permission prompt" { Why::Perm } else { Why::Ask }
        };
        let since = millis(&d, &["statusUpdatedAt", "updatedAt", "startedAt"]).unwrap_or(now * 1000.0) / 1000.0;
        let sid = d
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or_else(|| pid.to_string(), str::to_owned)
            .replace('-', "");
        let sid8: String = sid.chars().take(8).collect();
        let cwd = d.get("cwd").and_then(Value::as_str).unwrap_or("");
        let name = [
            metadata.map_or("", |s| s.title.as_str()),
            d.get("name").and_then(Value::as_str).unwrap_or(""),
            basename(cwd),
        ]
        .into_iter()
        .find(|s| !s.is_empty())
        .map_or_else(|| sid8.clone(), str::to_owned);
        if out.get(&sid).is_some_and(|old| old.since > since) {
            continue;
        }
        out.insert(
            sid,
            Found {
                id: sid8,
                name,
                kind: Kind::Claude,
                state,
                why,
                since,
                started: millis(&d, &["startedAt"]).unwrap_or(0.0) / 1000.0,
                session_id: d.get("sessionId").and_then(Value::as_str).unwrap_or("").to_owned(),
                project: cwd.to_owned(),
                transcript: None,
                target: metadata.and_then(|s| s.desktop_id.clone()).map(crate::scene::SessionTarget::Claude),
            },
        );
    }
    out.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::super::TestDir;
    use super::*;

    #[test]
    fn desktop_history_only_enriches_live_sessions_and_keeps_waiting_state() {
        let dir = TestDir::new();
        let raw = serde_json::json!({"sessionId":"local_ui-id", "cliSessionId":"live-cli-id", "title":"Moon Café", "isArchived":false});
        let metadata = desktop_session(raw.to_string().as_bytes()).unwrap();
        let mut desktop = HashMap::from([(metadata.id.clone(), metadata)]);
        assert!(sessions_at(1.0, &dir.0, &desktop, |_, _| true).is_empty());
        let status = serde_json::json!({"pid":42, "sessionId":"live-cli-id", "kind":"interactive",
            "status":"waiting", "waitingFor":"permission prompt", "name":"old title", "statusUpdatedAt":1000});
        fs::write(dir.0.join("42.json"), status.to_string()).unwrap();
        // Duplicate live records can exist while reconnecting a desktop session.
        fs::write(dir.0.join("43.json"), status.to_string()).unwrap();
        let found = sessions_at(2.0, &dir.0, &desktop, |pid, _| pid == 42);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].target.as_ref().and_then(|t| t.url()).as_deref(),
            Some("claude://code/continue?session=local_ui-id")
        );
        assert_eq!(found[0].name, "Moon Café");
        assert_eq!(found[0].state, State::Wait);
        assert_eq!(found[0].why, Why::Perm);
        assert!(sessions_at(2.0, &dir.0, &desktop, |_, _| false).is_empty());
        desktop.get_mut("live-cli-id").unwrap().archived = true;
        assert!(sessions_at(2.0, &dir.0, &desktop, |_, _| true).is_empty());
    }

    #[test]
    fn desktop_metadata_refreshes_titles_and_removes_deleted_history() {
        let dir = TestDir::new();
        let path = dir.0.join("session.json");
        fs::write(&path, r#"{"cliSessionId":"abc","title":"Moon"}"#).unwrap();
        let mut metadata = DesktopMetadata::default();
        assert_eq!(metadata.refresh(&dir.0)["abc"].title, "Moon");
        fs::write(&path, r#"{"cliSessionId":"abc","title":"Moon Café"}"#).unwrap();
        metadata.last = None;
        assert_eq!(metadata.refresh(&dir.0)["abc"].title, "Moon Café");
        fs::remove_file(path).unwrap();
        metadata.last = None;
        assert!(metadata.refresh(&dir.0).is_empty());
        assert!(desktop_session(br#"{"sessionId":"desktop-only"}"#).is_none());
    }
}
