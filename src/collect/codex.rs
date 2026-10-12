//! Codex: each running `codex` TUI process -> the rollout JSONL it has open
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

struct Process {
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
        if args.iter().find(|a| !a.starts_with('-')).is_some_and(|a| NOT_TUI.contains(&a.as_str())) {
            continue;
        }
        let mut rollouts: Vec<PathBuf> = process::open_files(pid)
            .into_iter()
            .filter(|t| {
                let b = t.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                t.starts_with(&sessions) && b.starts_with("rollout-") && b.ends_with(".jsonl")
            })
            .collect();
        rollouts.sort();
        rollouts.dedup();
        out.push(Process { pid, cwd: cwd.to_string_lossy().into_owned(), rollouts, start: process::start_epoch(pid) });
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
                let meta: Value = serde_json::from_slice(&first).unwrap_or(Value::Null);
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
                    session,
                };
                self.rollouts.insert(path.to_path_buf(), r.clone());
                Some(r)
            }
        }
    }

    pub fn sessions(&mut self, now: f64) -> Vec<Found> {
        let mut out = Vec::new();
        let mut used = HashSet::new();
        for p in processes() {
            let paths =
                if p.rollouts.is_empty() { thread_for(&p.cwd, p.start).into_iter().collect() } else { p.rollouts };
            let infos: Vec<Rollout> = paths.iter().filter_map(|x| self.rollout_info(x)).collect();
            used.extend(paths);
            let (uuid, state, since) = if infos.is_empty() {
                // a fresh TUI that has not started a thread yet
                (format!("{:08x}", p.pid), State::Idle, if p.start != 0.0 { p.start } else { now })
            } else {
                // The TUI keeps its thread and any compaction/sub-agent threads
                // open; it is working while any of them is mid-turn.
                let working: Vec<&Rollout> = infos.iter().filter(|i| i.state == State::Work).collect();
                let pool: Vec<&Rollout> = if working.is_empty() { infos.iter().collect() } else { working.clone() };
                let since = pool.iter().map(|i| i.since).fold(f64::NEG_INFINITY, f64::max);
                // the first of equals wins
                let newest = infos.iter().fold(&infos[0], |m, i| if i.since > m.since { i } else { m });
                let state = if working.is_empty() { State::Idle } else { State::Work };
                (newest.session.replace('-', ""), state, since)
            };
            let tail = |n: usize| -> String {
                let c: Vec<char> = uuid.chars().collect();
                c[c.len().saturating_sub(n)..].iter().collect()
            };
            let dir = basename(&p.cwd);
            out.push(Found {
                id: tail(8),
                name: format!("{}-{}", if dir.is_empty() { "codex" } else { dir }, tail(2)),
                kind: Kind::Codex,
                state,
                why: Why::None,
                since,
                started: p.start,
            });
        }
        self.rollouts.retain(|path, _| used.contains(path));
        out
    }
}
