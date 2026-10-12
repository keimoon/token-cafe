//! Transcript inspection for the focus panel and terminal diagnostics. No network calls, reasoning, tool output or raw commands are displayed.
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::Found;
use crate::scene::{Kind, State};
use crate::usage::claude_dir;

const INITIAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINE: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct Evidence {
    text: String,
    source: &'static str,
    timestamp: Option<String>,
    tool: Option<String>,
}
impl Evidence {
    fn json(&self) -> Value {
        json!({"text": self.text, "source": self.source, "timestamp": self.timestamp, "tool": self.tool})
    }
}

#[derive(Clone, Debug)]
struct Activity {
    evidence: Evidence,
    status: &'static str,
}

/// A diagnostic snapshot. Text is an excerpt, never a generated summary.
#[derive(Clone, Debug)]
pub struct SessionInfo(Value);
impl SessionInfo {
    pub fn display(&self, id: &str) -> crate::scene::SessionDetail {
        let v = &self.0;
        let task = v["task"]["text"].as_str().unwrap_or("TASK UNAVAILABLE").to_owned();
        crate::scene::SessionDetail { target: None, id: id.to_owned(), task }
    }

    pub fn json(&self) -> &Value {
        &self.0
    }
}

#[derive(Default)]
struct Transcript {
    offset: u64,
    identity: (u64, u64),
    modified: Option<SystemTime>,
    size: u64,
    anchor: Vec<u8>,
    partial: bool,
    task: Option<Evidence>,
    progress: Option<Evidence>,
    activity: Option<Activity>,
    pending: Vec<(String, Evidence)>,
    prompt_pending: bool,
}

fn excerpt(s: &str) -> String {
    // JSON escaping also protects the terminal; strip control/bidi formatting here.
    s.chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .filter(|c| !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

fn text_blocks(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return excerpt(s);
    }
    let Some(blocks) = v.as_array() else { return String::new() };
    excerpt(
        &blocks
            .iter()
            .filter_map(|b| {
                matches!(b["type"].as_str(), Some("text" | "input_text" | "output_text"))
                    .then(|| b["text"].as_str())
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn human_text(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('<')
        && !text.starts_with("# AGENTS.md instructions")
        && !text.starts_with("This session is being continued from a previous conversation")
}

fn timestamp(v: &Value) -> Option<String> {
    v["timestamp"].as_str().filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok()).map(str::to_owned)
}

fn evidence(text: String, source: &'static str, timestamp: Option<String>) -> Evidence {
    Evidence { text, source, timestamp, tool: None }
}

fn tool_label(name: &str, input: &Value) -> String {
    let target = input["file_path"]
        .as_str()
        .or_else(|| input["path"].as_str())
        .and_then(|s| Path::new(s).file_name())
        .map(|s| excerpt(&s.to_string_lossy()));
    let verb = match name {
        "Read" | "read_file" => "READING",
        "Edit" | "Write" | "edit_file" | "write_file" => "EDITING",
        "Grep" | "Glob" => return "SEARCHING FILES".into(),
        "apply_patch" => return "EDITING FILES".into(),
        "Bash" | "exec_command" => {
            let cmd = input["command"].as_str().or_else(|| input["cmd"].as_str()).unwrap_or("");
            // Recognize only simple, standalone commands, not scripts containing the word "test".
            if !cmd.contains(['\n', ';', '|', '&', '`', '$', '>', '<']) {
                let words: Vec<_> = cmd.split_whitespace().collect();
                if matches!(
                    words.as_slice(),
                    ["cargo" | "npm" | "pnpm", "test", ..] | ["pytest", ..] | ["go", "test", ..]
                ) {
                    return "RUNNING TESTS".into();
                }
                if matches!(words.as_slice(), ["cargo" | "npm" | "pnpm", "build", ..]) {
                    return "RUNNING BUILD".into();
                }
            }
            return "RUNNING COMMAND".into();
        }
        _ => return "USING TOOL".into(),
    };
    target.map_or_else(|| format!("{verb} FILE"), |p| format!("{verb} {p}"))
}

impl Transcript {
    fn request(&mut self, text: String, source: &'static str, ts: Option<String>) {
        if !human_text(&text) {
            return;
        }
        self.task = Some(evidence(text, source, ts));
        self.progress = None;
        self.activity = None;
        self.pending.clear();
        self.prompt_pending = true;
    }
    fn start(&mut self) {
        if !self.prompt_pending {
            self.task = None;
        }
        self.prompt_pending = false;
        self.progress = None;
        self.activity = None;
        self.pending.clear();
    }
    fn call(&mut self, id: &str, name: &str, input: &Value, ts: Option<String>) {
        self.prompt_pending = false;
        let mut ev = evidence(tool_label(name, input), "tool_call", ts);
        ev.tool = Some(excerpt(name));
        self.pending.retain(|(key, _)| key != id);
        // Bound retained metadata even with broken/missing tool results.
        if self.pending.len() == 64 {
            self.pending.remove(0);
        }
        self.pending.push((id.to_owned(), ev.clone()));
        self.activity = Some(Activity { evidence: ev, status: "observed" });
    }
    fn result(&mut self, id: &str, failed: bool, ts: Option<String>) {
        let Some(i) = self.pending.iter().position(|(key, _)| key == id) else { return };
        let (_, mut ev) = self.pending.remove(i);
        ev.source = "tool_result";
        ev.timestamp = ts;
        // A wrapper may return while its subprocess keeps running: never call this "finished".
        self.activity = Some(Activity { evidence: ev, status: if failed { "failed" } else { "returned" } });
        if let Some((_, ev)) = self.pending.last() {
            self.activity = Some(Activity { evidence: ev.clone(), status: "observed" });
        }
    }
    fn end(&mut self) {
        self.pending.clear();
        self.prompt_pending = false;
        if let Some(a) = &mut self.activity
            && a.status == "observed"
        {
            a.status = "ended_without_result";
        }
    }
    fn ingest(&mut self, v: &Value, kind: Kind, session: &str) {
        let ts = timestamp(v);
        if kind == Kind::Codex {
            let p = &v["payload"];
            match (v["type"].as_str(), p["type"].as_str()) {
                (Some("event_msg"), Some("task_started")) => self.start(),
                (Some("event_msg"), Some("task_complete" | "turn_aborted")) => self.end(),
                (Some("event_msg"), Some("user_message")) => {
                    self.request(excerpt(p["message"].as_str().unwrap_or("")), "user_message", ts);
                }
                (Some("response_item"), Some("message")) if p["role"] == "user" => {
                    self.request(text_blocks(&p["content"]), "user_message", ts);
                }
                (Some("response_item"), Some("message")) if p["role"] == "assistant" && p["phase"] == "commentary" => {
                    let text = text_blocks(&p["content"]);
                    if !text.is_empty() {
                        self.progress = Some(evidence(text, "assistant_commentary", ts));
                    }
                    self.prompt_pending = false;
                }
                (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
                    if let Some(id) = p["call_id"].as_str() {
                        let input = p["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .unwrap_or(Value::Null);
                        self.call(id, p["name"].as_str().unwrap_or(""), &input, ts);
                    }
                }
                (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
                    if let Some(id) = p["call_id"].as_str() {
                        self.result(id, false, ts);
                    }
                }
                _ => {}
            }
        } else {
            if v["isSidechain"] == true
                || v["isMeta"] == true
                || v["isCompactSummary"] == true
                || v["sessionId"].as_str().is_some_and(|id| id != session)
            {
                return;
            }
            let content = &v["message"]["content"];
            if v["type"] == "user" {
                let has_results = content.as_array().is_some_and(|b| b.iter().any(|b| b["type"] == "tool_result"));
                if !has_results && v.get("sourceToolAssistantUUID").is_none_or(Value::is_null) {
                    self.request(text_blocks(content), "user_message", ts.clone());
                }
            }
            if let Some(blocks) = content.as_array() {
                for b in blocks {
                    match (v["type"].as_str(), b["type"].as_str()) {
                        (Some("assistant"), Some("tool_use")) => {
                            if let Some(id) = b["id"].as_str() {
                                self.call(id, b["name"].as_str().unwrap_or(""), &b["input"], ts.clone());
                            }
                        }
                        (Some("user"), Some("tool_result")) => {
                            if let Some(id) = b["tool_use_id"].as_str() {
                                self.result(id, b["is_error"] == true, ts.clone());
                            }
                        }
                        (Some("assistant"), Some("text")) => {
                            let text = excerpt(b["text"].as_str().unwrap_or(""));
                            if !text.is_empty() {
                                self.progress = Some(evidence(text, "assistant_text", ts.clone()));
                            }
                            self.prompt_pending = false;
                        }
                        _ => {} // Never inspect thinking blocks.
                    }
                }
            }
        }
    }

    fn read(&mut self, path: &Path, kind: Kind, session: &str) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let identity = (meta.dev(), meta.ino());
        let modified = meta.modified().ok();
        let mut anchor = vec![0; self.anchor.len()];
        let anchor_ok = self.offset >= anchor.len() as u64 && {
            file.seek(SeekFrom::Start(self.offset - anchor.len() as u64))?;
            file.read_exact(&mut anchor).is_ok() && anchor == self.anchor
        };
        if identity != self.identity
            || meta.len() < self.offset
            || !anchor_ok
            || (meta.len() == self.size && modified != self.modified)
        {
            *self = Self::default();
            self.identity = identity;
            self.offset = meta.len().saturating_sub(INITIAL_BYTES);
            self.partial = self.offset > 0;
            file.seek(SeekFrom::Start(self.offset))?;
            if self.offset > 0 {
                let mut r = BufReader::new(&mut file);
                self.offset += r.skip_until(b'\n')? as u64;
            }
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file.take(meta.len().saturating_sub(self.offset)));
        loop {
            let mut line = Vec::new();
            let mut consumed = 0u64;
            let mut complete = false;
            loop {
                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    break;
                }
                let n = memchr::memchr(b'\n', buf).map_or(buf.len(), |i| {
                    complete = true;
                    i + 1
                });
                if line.len() + n <= MAX_LINE {
                    line.extend_from_slice(&buf[..n]);
                } else {
                    line.clear();
                    self.partial = true;
                }
                consumed += n as u64;
                reader.consume(n);
                if complete {
                    break;
                }
            }
            if !complete {
                break;
            } // Retry partial record on the next poll.
            self.offset += consumed;
            if consumed <= MAX_LINE as u64
                && let Ok(v) = serde_json::from_slice::<Value>(&line)
            {
                self.ingest(&v, kind, session);
            }
        }
        self.size = meta.len();
        self.modified = modified;
        let mut f = File::open(path)?;
        let n = self.offset.min(64);
        f.seek(SeekFrom::Start(self.offset - n))?;
        self.anchor.resize(n as usize, 0);
        f.read_exact(&mut self.anchor)?;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct InfoCache {
    files: HashMap<(PathBuf, String), Transcript>,
    claude_paths: HashMap<String, Vec<PathBuf>>,
    indexed: Option<Instant>,
}
impl InfoCache {
    fn claude_path(&mut self, id: &str) -> Option<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        if self.indexed.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            self.claude_paths.clear();
            // Only top-level project transcripts; never traverse subagents or parse saved history.
            if let Ok(projects) = fs::read_dir(claude_dir().join("projects")) {
                for p in projects.flatten() {
                    if !p.file_type().is_ok_and(|t| t.is_dir()) {
                        continue;
                    }
                    if let Ok(files) = fs::read_dir(p.path()) {
                        for f in files.flatten() {
                            let path = f.path();
                            if path.extension().is_some_and(|x| x == "jsonl")
                                && let Some(id) = path.file_stem().and_then(|s| s.to_str())
                            {
                                self.claude_paths.entry(id.to_owned()).or_default().push(path);
                            }
                        }
                    }
                }
            }
            self.indexed = Some(Instant::now());
        }
        let paths = self.claude_paths.get(id)?;
        // Ambiguous copies are safer to report as unavailable than attribute to the wrong project.
        (paths.len() == 1).then(|| paths[0].clone())
    }
    pub(super) fn collect(&mut self, found: &[Found]) -> Vec<SessionInfo> {
        let mut used = HashSet::new();
        let mut result = Vec::new();
        for f in found {
            let path = if f.kind == Kind::Claude { self.claude_path(&f.session_id) } else { f.transcript.clone() };
            let key = path.as_ref().map(|p| (p.clone(), f.session_id.clone()));
            let transcript = key.as_ref().and_then(|key| {
                used.insert(key.clone());
                let t = self.files.entry(key.clone()).or_default();
                t.read(&key.0, f.kind, &f.session_id).ok().map(|_| t)
            });
            let (task, progress, activity, partial) = if let Some(t) = transcript {
                let activity = t.activity.as_ref().map(|a| {
                    let mut v = a.evidence.json();
                    v["status"] = json!(if f.state != State::Work && a.status == "observed" {
                        "ended_without_result"
                    } else {
                        a.status
                    });
                    v
                });
                (
                    t.task.as_ref().map(Evidence::json),
                    t.progress.as_ref().map(Evidence::json),
                    activity,
                    Some(t.partial),
                )
            } else {
                (None, None, None, None)
            };
            result.push(SessionInfo(json!({
                "id":f.session_id, "kind": if f.kind==Kind::Codex {"codex"} else {"claude"},
                "title":excerpt(&f.name), "project":f.project,
                "state":match f.state {State::Work=>"work",State::Idle=>"idle",State::Wait=>"wait"},
                "state_since":f.since,
                "task":task, "task_scope":if f.state==State::Idle {"last_request"} else {"latest_observed_request"},
                "progress":progress, "last_activity":activity,
                "fallback":match f.state {State::Work=>"WORKING",State::Idle=>"IDLE",State::Wait=>"WAITING FOR YOU"},
                "transcript":path, "transcript_available":partial.is_some(), "history_partial":partial,
            })));
        }
        self.files.retain(|key, _| used.contains(key));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::TestDir;
    use crate::scene::Why;
    use std::io::Write;

    const TS: &str = "2026-10-12T06:00:00Z";
    fn codex(t: &str, payload: Value) -> Value {
        json!({"type":t,"timestamp":TS,"payload":payload})
    }
    fn user(text: &str) -> Value {
        codex("response_item", json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}))
    }
    fn call(id: &str, tool: &str, args: Value) -> Value {
        codex("response_item", json!({"type":"function_call","call_id":id,"name":tool,"arguments":args.to_string()}))
    }
    fn result(id: &str) -> Value {
        codex("response_item", json!({"type":"function_call_output","call_id":id,"output":"secret output"}))
    }
    fn event(name: &str) -> Value {
        codex("event_msg", json!({"type":name}))
    }
    fn ingest(t: &mut Transcript, v: Value) {
        t.ingest(&v, Kind::Codex, "root");
    }
    fn write(path: &Path, values: &[Value]) {
        fs::write(path, values.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    }
    fn found(path: &Path) -> Found {
        Found {
            target: None,
            id: "root".into(),
            session_id: "root".into(),
            name: "Moon Café".into(),
            project: "/projects/moon".into(),
            transcript: Some(path.to_owned()),
            kind: Kind::Codex,
            state: State::Work,
            why: Why::None,
            since: 1.,
            started: 1.,
        }
    }

    #[test]
    fn codex_turn_reset_and_results_do_not_claim_command_success() {
        let mut t = Transcript::default();
        ingest(&mut t, event("task_started"));
        ingest(&mut t, user("Fix login redirect"));
        ingest(&mut t, call("a", "exec_command", json!({"cmd":"cargo test --locked"})));
        assert_eq!(t.activity.as_ref().unwrap().evidence.text, "RUNNING TESTS");
        assert_eq!(t.activity.as_ref().unwrap().status, "observed");
        ingest(&mut t, result("a"));
        assert_eq!(t.activity.as_ref().unwrap().status, "returned");
        assert!(t.pending.is_empty());
        ingest(&mut t, event("task_complete"));
        assert_eq!(t.task.as_ref().unwrap().text, "Fix login redirect");
        ingest(&mut t, event("task_started"));
        assert!(t.task.is_none() && t.activity.is_none() && t.progress.is_none());
        // Other versions log the human message before task_started.
        ingest(&mut t, user("Add tax rounding"));
        ingest(&mut t, event("task_started"));
        assert_eq!(t.task.as_ref().unwrap().text, "Add tax rounding");
    }

    #[test]
    fn overlapping_calls_and_aborted_turns_keep_honest_status() {
        let mut t = Transcript::default();
        ingest(&mut t, call("a", "Read", json!({"file_path":"/private/project/auth.rs"})));
        ingest(&mut t, call("b", "Edit", json!({"file_path":"/private/project/ui.rs","new_string":"secret"})));
        ingest(&mut t, result("unknown"));
        assert_eq!(t.pending.len(), 2);
        ingest(&mut t, result("b"));
        assert_eq!(t.activity.as_ref().unwrap().evidence.text, "READING auth.rs");
        ingest(&mut t, event("turn_aborted"));
        assert_eq!(t.activity.as_ref().unwrap().status, "ended_without_result");
        assert!(t.pending.is_empty());
    }

    #[test]
    fn only_public_codex_messages_are_retained() {
        let mut t = Transcript::default();
        ingest(&mut t, user("Implement the moon dashboard"));
        ingest(&mut t, user("<environment_context>private context</environment_context>"));
        ingest(
            &mut t,
            codex("response_item", json!({"type":"message","role":"developer","content":"private instruction"})),
        );
        ingest(&mut t, codex("response_item", json!({"type":"reasoning","summary":"private thought"})));
        ingest(
            &mut t,
            codex(
                "response_item",
                json!({"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking the layout"}]}),
            ),
        );
        assert_eq!(t.task.unwrap().text, "Implement the moon dashboard");
        assert_eq!(t.progress.unwrap().text, "Checking the layout");
    }

    #[test]
    fn claude_tool_results_and_sidechains_are_not_user_requests() {
        let mut t = Transcript::default();
        let mut add = |ty: &str, content: Value| {
            t.ingest(
                &json!({"type":ty,"sessionId":"root","timestamp":TS,"message":{"content":content}}),
                Kind::Claude,
                "root",
            )
        };
        add("user", json!([{"type":"text","text":"Repair export"}]));
        add(
            "assistant",
            json!([{"type":"thinking","thinking":"private thought"},{"type":"text","text":"Checking export"},{"type":"tool_use","id":"a","name":"Edit","input":{"file_path":"/projects/moon/export.rs","new_string":"secret"}}]),
        );
        add("user", json!([{"type":"tool_result","tool_use_id":"a","is_error":true,"content":"private output"}]));
        t.ingest(
            &json!({"type":"user","isSidechain":true,"sessionId":"root","message":{"content":"Child task"}}),
            Kind::Claude,
            "root",
        );
        t.ingest(&json!({"type":"user","sessionId":"other","message":{"content":"Other task"}}), Kind::Claude, "root");
        t.ingest(
            &json!({"type":"user","isCompactSummary":true,"message":{"content":"Summary text"}}),
            Kind::Claude,
            "root",
        );
        assert_eq!(t.task.unwrap().text, "Repair export");
        assert_eq!(t.progress.unwrap().text, "Checking export");
        let a = t.activity.unwrap();
        assert_eq!(a.evidence.text, "EDITING export.rs");
        assert_eq!(a.status, "failed");
    }

    #[test]
    fn incremental_reader_retries_partial_records_and_skips_bad_lines() {
        let dir = TestDir::new();
        let path = dir.0.join("log.jsonl");
        write(&path, &[user("Fix the moon UI")]);
        let mut t = Transcript::default();
        t.read(&path, Kind::Codex, "root").unwrap();
        let offset = t.offset;
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.offset, offset);
        let record = call("a", "Read", json!({"file_path":"view.rs"})).to_string();
        let half = record.len() / 2;
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&record.as_bytes()[..half]).unwrap();
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.offset, offset);
        assert!(t.activity.is_none());
        f.write_all(&record.as_bytes()[half..]).unwrap();
        f.write_all(b"\nnot json\n").unwrap();
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.activity.as_ref().unwrap().evidence.text, "READING view.rs");
        assert_eq!(t.offset, fs::metadata(path).unwrap().len());
    }

    #[test]
    fn replacement_truncation_and_regrowth_reset_cached_evidence() {
        let dir = TestDir::new();
        let path = dir.0.join("log.jsonl");
        write(&path, &[user("Old request"), call("a", "Read", json!({"path":"old.rs"}))]);
        let mut t = Transcript::default();
        t.read(&path, Kind::Codex, "root").unwrap();
        write(&path, &[user("New request")]);
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.task.as_ref().unwrap().text, "New request");
        assert!(t.activity.is_none());
        let next = dir.0.join("replacement");
        write(&next, &[user("Replacement request")]);
        fs::rename(next, &path).unwrap();
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.task.as_ref().unwrap().text, "Replacement request");
        write(&path, &[user("A rewritten and much longer request"), result("a")]);
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.task.as_ref().unwrap().text, "A rewritten and much longer request");
        assert!(t.activity.is_none());
    }

    #[test]
    fn large_history_and_oversize_records_are_bounded_and_marked_partial() {
        let dir = TestDir::new();
        let path = dir.0.join("log.jsonl");
        let mut f = File::create(&path).unwrap();
        f.write_all(&vec![b'x'; INITIAL_BYTES as usize + 100]).unwrap();
        f.write_all(b"\n").unwrap();
        writeln!(f, "{}", user("Recent request")).unwrap();
        let mut t = Transcript::default();
        t.read(&path, Kind::Codex, "root").unwrap();
        assert!(t.partial);
        assert_eq!(t.task.as_ref().unwrap().text, "Recent request");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&vec![b'x'; MAX_LINE + 10]).unwrap();
        f.write_all(b"\n").unwrap();
        writeln!(f, "{}", call("a", "Read", json!({"path":"recent.rs"}))).unwrap();
        t.read(&path, Kind::Codex, "root").unwrap();
        assert_eq!(t.activity.as_ref().unwrap().evidence.text, "READING recent.rs");
    }

    #[test]
    fn missing_and_closed_transcripts_do_not_leak_cached_details() {
        let dir = TestDir::new();
        let path = dir.0.join("root.jsonl");
        write(&path, &[user("Fix the moon UI"), call("a", "Read", json!({"path":"ui.rs"}))]);
        let mut cache = InfoCache::default();
        let mut f = found(&path);
        let work = cache.collect(&[found(&path)]);
        assert_eq!(work[0].json()["last_activity"]["status"], "observed");
        f.state = State::Idle;
        let idle = cache.collect(&[f]);
        assert_eq!(idle[0].json()["last_activity"]["status"], "ended_without_result");
        assert_eq!(idle[0].json()["task_scope"], "last_request");
        fs::remove_file(&path).unwrap();
        let unavailable = cache.collect(&[found(&path)]);
        assert_eq!(unavailable[0].json()["transcript_available"], false);
        assert!(unavailable[0].json()["task"].is_null());
        assert!(cache.collect(&[]).is_empty());
        assert!(cache.files.is_empty());
    }

    #[test]
    fn claude_path_mapping_requires_unique_exact_session_identity() {
        let mut cache = InfoCache { indexed: Some(Instant::now()), ..Default::default() };
        cache.claude_paths.insert("root".into(), vec![PathBuf::from("/projects/moon/root.jsonl")]);
        assert!(cache.claude_path("missing").is_none());
        assert!(cache.claude_path("../root").is_none());
        assert_eq!(cache.claude_path("root").unwrap(), Path::new("/projects/moon/root.jsonl"));
        cache.claude_paths.get_mut("root").unwrap().push(PathBuf::from("/projects/copy/root.jsonl"));
        assert!(cache.claude_path("root").is_none());
    }

    #[test]
    fn tool_labels_do_not_expose_commands_or_guess_complex_scripts() {
        assert_eq!(tool_label("Bash", &json!({"command":"echo cargo test; curl private"})), "RUNNING COMMAND");
        assert_eq!(tool_label("Bash", &json!({"command":"cargo build --release"})), "RUNNING BUILD");
        assert_eq!(tool_label("exec", &json!({"input":"arbitrary code"})), "USING TOOL");
        assert_eq!(excerpt("hello\u{1b}[31m\nworld\u{202e}"), "hello[31m world");
        assert_eq!(excerpt(&"é".repeat(200)).chars().count(), 160);
    }
}
