//! Codex: CLI sessions and desktop app-server chats -> their open rollout JSONL files.
//! Desktop servers hold multiple chats; each root session gets its own patron.
//! Each running `codex` TUI process -> the rollout JSONL it has open
//! (or the newest thread for its cwd in ~/.codex/state_5.sqlite) -> last
//! task_started (work) vs task_complete/turn_aborted (idle). Codex logs no
//! approval requests, so Codex never shows "wait".

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

use super::{Found, basename, process};
use crate::scene::{Kind, State, Why};
use crate::usage::codex_dir;

/// codex subcommands that are not an interactive session a human answers
const NOT_TUI: &[&str] = &[
    "exec",
    "e",
    "exec-server",
    "app-server",
    "mcp-server",
    "mcp",
    "login",
    "logout",
    "completion",
    "debug",
    "apply",
    "a",
    "cloud",
    "sandbox",
    "proto",
    "features",
    "app",
    "help",
];

/// A long turn writes megabytes after its task_started (377 KB in 11
/// minutes was seen), so no fixed tail is enough: the first look scans back
/// until a task event turns up, later looks read only what Codex appended.
const SCAN_CHUNK: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Tui,
    Desktop,
}

/// Skip global option values before interpreting the subcommand. In particular,
/// desktop starts `codex -c key=value app-server`, not always `codex app-server`.
fn mode(args: &[String]) -> Option<Mode> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            return Some(Mode::Tui);
        }
        if matches!(
            arg.as_str(),
            "-c" | "--config"
                | "-m"
                | "--model"
                | "-p"
                | "--profile"
                | "-C"
                | "--cd"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "-i"
                | "--image"
                | "--enable"
                | "--disable"
                | "--add-dir"
                | "--local-provider"
        ) {
            args.next()?;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return match arg.as_str() {
            "app-server" => Some(Mode::Desktop),
            command if NOT_TUI.contains(&command) => None,
            _ => Some(Mode::Tui),
        };
    }
    Some(Mode::Tui)
}

struct Process {
    mode: Mode,
    pid: u32,
    cwd: String,
    rollouts: Vec<PathBuf>,
    start: f64,
}

/// Per rollout file: where reading stopped, the state found so far and the
/// session it belongs to.
#[derive(Clone)]
struct Rollout {
    offset: u64,
    state: State,
    since: f64,
    session: String,
    cwd: String,
    started: f64,
    child: bool,
}

#[derive(Default)]
pub struct Codex {
    rollouts: HashMap<PathBuf, Rollout>,
}

fn processes() -> Vec<Process> {
    let sessions = codex_dir().join("sessions");
    let mut out = Vec::new();
    for pid in process::pids() {
        let Some(exe) = process::exe(pid) else { continue };
        let exe = exe.to_string_lossy();
        if basename(exe.strip_suffix(" (deleted)").unwrap_or(&exe)) != "codex" {
            continue;
        }
        let (Some(args), Some(cwd)) = (process::args(pid), process::cwd(pid)) else { continue };
        let Some(mode) = mode(&args) else { continue };
        let mut rollouts: Vec<PathBuf> = process::open_files(pid)
            .into_iter()
            .filter(|t| {
                let b = t.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                t.starts_with(&sessions) && b.starts_with("rollout-") && b.ends_with(".jsonl")
            })
            .collect();
        rollouts.sort();
        rollouts.dedup();
        out.push(Process {
            mode,
            pid,
            cwd: cwd.to_string_lossy().into_owned(),
            rollouts,
            start: process::start_epoch(pid),
        });
    }
    out
}

/// Newest thread for this cwd that changed since the process started.
fn thread_for(cwd: &str, start: f64) -> Option<PathBuf> {
    let db = codex_dir().join("state_5.sqlite");
    let con =
        Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    con.busy_timeout(std::time::Duration::from_secs(1)).ok()?;
    con.query_row(
        "SELECT rollout_path FROM threads WHERE cwd = ? AND archived = 0 AND updated_at_ms >= ? \
         AND coalesce(thread_source, 'user') = 'user' ORDER BY updated_at_ms DESC LIMIT 1",
        (cwd, ((start - 5.0) * 1000.0) as i64),
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(PathBuf::from)
}

fn parse_ts(s: Option<&Value>) -> Option<f64> {
    let s = s?.as_str()?;
    let secs = |t: i64, ns: u32| t as f64 + ns as f64 / 1e9;
    match DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Some(secs(dt.timestamp(), dt.timestamp_subsec_nanos())),
        Err(_) => {
            let dt = Local
                .from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?)
                .earliest()?;
            Some(secs(dt.timestamp(), dt.timestamp_subsec_nanos()))
        }
    }
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

/// work|idle and when, from a task event line.
type TaskEvent = (State, Option<f64>);

/// The task event on this line, if it is one.
fn task_event(raw: &[u8]) -> Option<TaskEvent> {
    if !has(raw, b"\"event_msg\"")
        || !(has(raw, b"task_started") || has(raw, b"task_complete") || has(raw, b"turn_aborted"))
    {
        return None;
    }
    let o: Value = serde_json::from_slice(raw).ok()?;
    let ts = parse_ts(o.get("timestamp"));
    match o.get("payload")?.get("type")?.as_str()? {
        "task_started" => Some((State::Work, ts)),
        "task_complete" | "turn_aborted" => Some((State::Idle, ts)),
        _ => None,
    }
}

/// -> (last task event or None, offset just past the last complete line).
fn scan_back(f: &mut File, size: u64) -> std::io::Result<(Option<TaskEvent>, u64)> {
    let start0 = size.saturating_sub(SCAN_CHUNK);
    f.seek(SeekFrom::Start(start0))?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail)?;
    let done = memchr::memrchr(b'\n', &tail).map_or(0, |nl| start0 + nl as u64 + 1);
    let (mut pos, mut carry) = (done, Vec::new());
    while pos > 0 {
        let start = pos.saturating_sub(SCAN_CHUNK);
        f.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0; (pos - start) as usize];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&carry);
        // a line cut at `start` is finished with the next chunk back
        let rest = if start > 0 {
            match memchr::memchr(b'\n', &chunk) {
                Some(i) => {
                    carry = chunk[..i].to_vec();
                    &chunk[i + 1..]
                }
                None => {
                    carry = chunk.clone();
                    &chunk[chunk.len()..]
                }
            }
        } else {
            &chunk[..]
        };
        if let Some(ev) = rest.split(|&b| b == b'\n').rev().find_map(task_event) {
            return Ok((Some(ev), done));
        }
        pos = start;
    }
    Ok((None, done))
}

fn mtime(meta: &fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64())
}

impl Codex {
    fn rollout_info(&mut self, path: &Path) -> Option<Rollout> {
        let meta = fs::metadata(path).ok()?;
        let (size, mtime) = (meta.len(), mtime(&meta));
        let mut f = File::open(path).ok()?;
        match self.rollouts.get_mut(path) {
            Some(r) if size >= r.offset => {
                if size > r.offset {
                    f.seek(SeekFrom::Start(r.offset)).ok()?;
                    let mut data = vec![0; (size - r.offset) as usize];
                    f.read_exact(&mut data).ok()?;
                    // leave a half-written last line for next time
                    let end = memchr::memrchr(b'\n', &data).map_or(0, |i| i + 1);
                    for raw in data[..end].split(|&b| b == b'\n') {
                        if let Some((state, ts)) = task_event(raw) {
                            r.state = state;
                            r.since = ts.filter(|&t| t != 0.0).unwrap_or(mtime);
                        }
                    }
                    r.offset += end as u64;
                }
                Some(r.clone())
            }
            _ => {
                let mut first = Vec::new();
                BufReader::new(&mut f).take(1 << 20).read_until(b'\n', &mut first).ok()?;
                let meta: Value = serde_json::from_slice(&first).ok()?;
                if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
                    return None;
                }
                // a compaction or sub-agent thread carries its parent's session_id
                let session = match meta.get("payload").and_then(|p| p.get("session_id")).and_then(Value::as_str) {
                    Some(s) if !s.is_empty() => s.to_string(),
                    _ => {
                        let b = basename(path.to_str().unwrap_or(""));
                        let stem: Vec<char> = b.strip_suffix(".jsonl").unwrap_or(b).chars().collect();
                        stem[stem.len().saturating_sub(36)..].iter().collect()
                    }
                };
                let (ev, done) = scan_back(&mut f, size).ok()?;
                let r = Rollout {
                    offset: done,
                    state: ev.map_or(State::Idle, |e| e.0),
                    since: ev.and_then(|e| e.1).filter(|&t| t != 0.0).unwrap_or(mtime),
                    cwd: meta.pointer("/payload/cwd").and_then(Value::as_str).unwrap_or("").to_owned(),
                    started: parse_ts(meta.pointer("/payload/timestamp")).unwrap_or(0.0),
                    child: meta.pointer("/payload/source/subagent").is_some()
                        || meta.pointer("/payload/thread_source").and_then(Value::as_str) == Some("subagent"),
                    session,
                };
                self.rollouts.insert(path.to_path_buf(), r.clone());
                Some(r)
            }
        }
    }

    pub fn sessions(&mut self, now: f64) -> Vec<Found> {
        let db = Connection::open_with_flags(codex_dir().join("state_5.sqlite"), OpenFlags::SQLITE_OPEN_READ_ONLY).ok();
        self.collect(now, processes(), db.as_ref())
    }

    fn collect(&mut self, now: f64, mut processes: Vec<Process>, db: Option<&Connection>) -> Vec<Found> {
        // Prefer desktop titles when the same root is also held by a CLI process.
        processes.sort_by_key(|p| (p.mode == Mode::Tui, p.pid));
        let mut out: HashMap<String, Found> = HashMap::new();
        let mut used = HashSet::new();
        for p in processes {
            // No cwd fallback for servers: their cwd is the app's Resources folder,
            // and historical DB rows do not establish that a chat is loaded.
            let paths = if p.rollouts.is_empty() && p.mode == Mode::Tui {
                thread_for(&p.cwd, p.start).into_iter().collect()
            } else {
                p.rollouts
            };
            let infos: Vec<Rollout> = paths.iter().filter_map(|x| self.rollout_info(x)).collect();
            used.extend(paths);
            if p.mode == Mode::Desktop {
                for r in infos.iter().filter(|r| !r.child) {
                    // A root's task lifecycle is authoritative. Completed root turns
                    // must not be kept busy by an older, unfinished subagent log.
                    let metadata = db.and_then(|db| desktop_thread(db, &r.session));
                    if metadata.as_ref().is_some_and(|(_, _, archived)| *archived) {
                        continue;
                    }
                    let cwd = metadata.as_ref().map_or(r.cwd.as_str(), |(_, cwd, _)| cwd.as_str());
                    let name = metadata
                        .as_ref()
                        .map(|(title, _, _)| title.as_str())
                        .filter(|t| !t.is_empty())
                        .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect())
                        .unwrap_or_else(|| session_name(cwd, &r.session));
                    let found = Found {
                        id: short_id(&r.session),
                        name,
                        kind: Kind::Codex,
                        state: r.state,
                        why: Why::None,
                        since: r.since,
                        started: r.started,
                    };
                    insert_session(&mut out, &r.session, found);
                }
                continue;
            }
            let (uuid, state, since) = if infos.is_empty() {
                (format!("{:08x}", p.pid), State::Idle, if p.start != 0.0 { p.start } else { now })
            } else {
                let working: Vec<&Rollout> = infos.iter().filter(|i| i.state == State::Work).collect();
                let pool: Vec<&Rollout> = if working.is_empty() { infos.iter().collect() } else { working.clone() };
                let since = pool.iter().map(|i| i.since).fold(f64::NEG_INFINITY, f64::max);
                let newest = infos.iter().fold(&infos[0], |m, i| if i.since > m.since { i } else { m });
                let state = if working.is_empty() { State::Idle } else { State::Work };
                (newest.session.clone(), state, since)
            };
            let found = Found {
                id: short_id(&uuid),
                name: session_name(&p.cwd, &uuid),
                kind: Kind::Codex,
                state,
                why: Why::None,
                since,
                started: p.start,
            };
            insert_session(&mut out, &uuid, found);
        }
        self.rollouts.retain(|path, _| used.contains(path));
        out.into_values().collect()
    }
}

/// New desktop versions store the display name separately from the initial
/// prompt (`title`). Fall back for older SQLite schemas without `name`.
fn desktop_thread(db: &Connection, id: &str) -> Option<(String, String, bool)> {
    let read = |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?, row.get(2)?));
    db.query_row("SELECT coalesce(nullif(name, ''), title), cwd, archived FROM threads WHERE id = ?", [id], read)
        .or_else(|_| db.query_row("SELECT title, cwd, archived FROM threads WHERE id = ?", [id], read))
        .ok()
}

fn short_id(uuid: &str) -> String {
    let id = uuid.replace('-', "");
    id.chars().skip(id.chars().count().saturating_sub(8)).collect()
}

fn session_name(cwd: &str, uuid: &str) -> String {
    let dir = basename(cwd);
    let id = short_id(uuid);
    let suffix: String = id.chars().skip(id.chars().count().saturating_sub(2)).collect();
    format!("{}-{suffix}", if dir.is_empty() { "codex" } else { dir })
}

fn insert_session(out: &mut HashMap<String, Found>, uuid: &str, found: Found) {
    match out.entry(uuid.to_owned()) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(found);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if found.since > entry.get().since {
                entry.insert(found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::TestDir;
    use super::*;
    use std::io::Write;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classifies_servers_after_global_options_and_excludes_helpers() {
        assert_eq!(mode(&args(&["-c", "foo=true", "app-server", "--listen", "stdio://"])), Some(Mode::Desktop));
        assert_eq!(mode(&args(&["--config=foo=true", "app-server"])), Some(Mode::Desktop));
        for command in ["exec-server", "exec", "sandbox", "mcp-server"] {
            assert_eq!(mode(&args(&["--enable", "feature", command])), None);
        }
        assert_eq!(mode(&args(&["--cd", "/a project", "resume"])), Some(Mode::Tui));
        assert_eq!(mode(&args(&["--", "app-server"])), Some(Mode::Tui));
        assert_eq!(mode(&[]), Some(Mode::Tui));
        assert_eq!(mode(&args(&["-c"])), None);
    }

    fn rollout(dir: &Path, id: &str, parent: Option<&str>, event: &str) -> PathBuf {
        let path = dir.join(format!("rollout-{id}.jsonl"));
        let meta = serde_json::json!({"type":"session_meta", "payload": {
            "id": id, "session_id": parent.unwrap_or(id), "cwd": "/projects/moon-cafe",
            "timestamp": "2026-10-12T01:00:00Z", "thread_source": if parent.is_some() { "subagent" } else { "user" }
        }});
        fs::write(&path, format!("{meta}\n{}\n", event_line(event, "2026-10-12T01:01:00Z"))).unwrap();
        path
    }

    fn event_line(event: &str, timestamp: &str) -> String {
        serde_json::json!({"type":"event_msg", "timestamp":timestamp,"payload":{"type":event}}).to_string()
    }

    fn server(paths: &[PathBuf]) -> Process {
        Process {
            mode: Mode::Desktop,
            pid: 1,
            cwd: "/Applications/App.app/Contents/Resources".into(),
            rollouts: paths.to_vec(),
            start: 1.0,
        }
    }

    #[test]
    fn desktop_chats_are_separate_deduplicated_and_follow_their_own_lifecycle() {
        let dir = TestDir::new();
        let a = rollout(&dir.0, "root-00000001", None, "task_started");
        let b = rollout(&dir.0, "root-00000002", None, "task_complete");
        let archived = rollout(&dir.0, "root-00000003", None, "task_started");
        let child = rollout(&dir.0, "child", Some("root-00000002"), "task_started");
        let orphan = rollout(&dir.0, "orphan", Some("absent-root"), "task_started");
        let paths = vec![a.clone(), b.clone(), archived, child, orphan];
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, title TEXT, cwd TEXT, archived INTEGER);
            INSERT INTO threads VALUES('root-00000001','Moon Café','/projects/moon-cafe',0),
            ('root-00000002','Star Charts','/projects/star-charts',0),
            ('root-00000003','Archived','/projects/archive',1);",
        )
        .unwrap();
        let mut collector = Codex::default();
        let found = collector.collect(1.0, vec![server(&paths), server(&paths)], Some(&db));
        assert_eq!(found.len(), 2);
        let a_found = found.iter().find(|f| f.name == "Moon Café").unwrap();
        let b_found = found.iter().find(|f| f.name == "Star Charts").unwrap();
        assert_eq!(a_found.state, State::Work);
        assert_eq!(b_found.state, State::Idle); // stale child does not keep parent busy
        assert!(a_found.started > 0.0);

        let mut file = fs::OpenOptions::new().append(true).open(&a).unwrap();
        writeln!(file, "{}", event_line("task_complete", "2026-10-12T01:02:00Z")).unwrap();
        let found = collector.collect(1.0, vec![server(std::slice::from_ref(&a))], Some(&db));
        assert_eq!(found.len(), 1); // closed logs no longer produce patrons
        assert_eq!(found[0].state, State::Idle);
        assert_eq!(collector.rollouts.len(), 1);
        assert!(collector.collect(1.0, vec![server(&[])], Some(&db)).is_empty());
        assert!(collector.rollouts.is_empty());
    }

    #[test]
    fn desktop_uses_display_name_instead_of_initial_prompt() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE threads(id TEXT, name TEXT, title TEXT, cwd TEXT, archived INTEGER);
            INSERT INTO threads VALUES('root','Moon Café','A long initial request','/projects/moon',0);",
        )
        .unwrap();
        assert_eq!(desktop_thread(&db, "root").unwrap().0, "Moon Café");
        db.execute("UPDATE threads SET name = ''", []).unwrap();
        assert_eq!(desktop_thread(&db, "root").unwrap().0, "A long initial request");
    }

    #[test]
    fn desktop_falls_back_to_metadata_and_retries_partial_headers() {
        let dir = TestDir::new();
        let path = dir.0.join("rollout-test.jsonl");
        fs::write(&path, "{\"type\":").unwrap();
        let mut collector = Codex::default();
        assert!(collector.collect(1.0, vec![server(std::slice::from_ref(&path))], None).is_empty());
        let valid = rollout(&dir.0, "test", None, "task_started");
        assert_eq!(path, valid);
        let found = collector.collect(1.0, vec![server(&[valid])], None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "moon-cafe-st");
    }

    #[test]
    fn cli_and_desktop_same_session_appear_once_with_desktop_title() {
        let dir = TestDir::new();
        let path = rollout(&dir.0, "root", None, "task_started");
        let mut cli = server(std::slice::from_ref(&path));
        cli.mode = Mode::Tui;
        let mut collector = Codex::default();
        let found = collector.collect(1.0, vec![cli, server(&[path])], None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "moon-cafe-ot");
    }
}
