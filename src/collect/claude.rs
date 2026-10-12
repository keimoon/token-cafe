//! Claude Code: ~/.claude/sessions/<pid>.json, one per interactive session.
//! status busy -> work, idle -> idle, waiting -> wait (waitingFor
//! "permission prompt" -> "!", anything else -> "?").

use std::fs;

use serde_json::Value;

use super::{Found, basename, process, truthy};
use crate::scene::{Kind, State, Why};
use crate::usage::claude_dir;

fn millis(d: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| d.get(*k).filter(|v| truthy(v)).and_then(Value::as_f64))
}

pub fn sessions(now: f64) -> Vec<Found> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(claude_dir().join("sessions")) else { return out };
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
        if !process::matches_session(pid, &d) {
            continue; // dead process or a reused pid
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
        let name = [d.get("name").and_then(Value::as_str).unwrap_or(""), basename(cwd)]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or_else(|| sid8.clone(), str::to_owned);
        out.push(Found {
            id: sid8,
            name,
            kind: Kind::Claude,
            state,
            why,
            since,
            started: millis(&d, &["startedAt"]).unwrap_or(0.0) / 1000.0,
        });
    }
    out
}
