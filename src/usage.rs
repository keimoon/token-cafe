//! This calendar month's tokens and list-price dollars for the tab, Claude
//! Code and Codex summed.
//!
//! Both CLIs keep a full transcript of every session on disk, and every
//! assistant turn carries the token usage the API reported for it:
//!
//!   Claude   ~/.claude/projects/<slug>/<session>.jsonl   ($CLAUDE_CONFIG_DIR)
//!            assistant records -> message.usage, message.model
//!   Codex    ~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl   ($CODEX_HOME)
//!            token_usage_record events (new), token_count events (older)
//!
//! The dollars are what the same traffic would have cost on each vendor's
//! API at list price: a measure of what was consumed, not of what was billed.
//! Per-file results are cached by size and mtime, so a re-count only re-reads
//! the transcripts that changed.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{Value, json};

const M: f64 = 1_000_000.0;
const CACHE_VERSION: u64 = 1;

/* ---- price list, US dollars per million tokens ---------------------------
 * Claude: cache writes are 1.25x input for the 5 minute TTL and 2x for the
 * 1 hour TTL; cache reads are 0.1x input, except Fable 5.1 at 0.025x.
 * Codex: cached input and cache writes are quoted directly by OpenAI. */

/// model prefix -> (input, output, cache read)
const CLAUDE_PRICES: &[(&str, [f64; 3])] = &[
    ("claude-fable-5-1", [10.00, 50.00, 0.25]),
    ("claude-mythos-5-1", [10.00, 50.00, 0.25]),
    ("claude-fable-5", [10.00, 50.00, 1.00]),
    ("claude-opus-5-5", [4.00, 20.00, 0.20]),
    ("claude-opus-5", [5.00, 25.00, 0.50]),
    ("claude-opus-4-8", [5.00, 25.00, 0.50]),
    ("claude-opus-4-7", [5.00, 25.00, 0.50]),
    ("claude-opus-4-6", [5.00, 25.00, 0.50]),
    ("claude-sonnet-5-5", [2.00, 10.00, 0.20]),
    ("claude-sonnet-5", [2.00, 10.00, 0.20]),
    ("claude-sonnet-4-6", [3.00, 15.00, 0.30]),
    ("claude-haiku-4-5", [1.00, 5.00, 0.10]),
];

/// model -> (input, output, cached input, cache write)
const CODEX_PRICES: &[(&str, [f64; 4])] = &[
    ("gpt-6-astra", [10.00, 50.00, 1.00, 12.50]),
    ("gpt-5.6-cyber", [12.50, 75.00, 1.25, 15.625]),
    ("gpt-5.6-sol", [4.00, 20.00, 0.40, 5.00]),
    ("gpt-5.6-terra", [2.00, 12.00, 0.20, 2.50]),
    ("gpt-5.6-luna", [0.20, 1.20, 0.02, 0.25]),
    ("gpt-5.5-pro", [30.00, 180.00, 30.00, 37.50]),
    ("gpt-5.5", [5.00, 30.00, 0.50, 6.25]),
    ("gpt-5.4-mini", [0.75, 4.50, 0.075, 0.9375]),
    ("gpt-5.4-nano", [0.20, 1.25, 0.02, 0.25]),
    ("gpt-5.4", [2.50, 15.00, 0.25, 3.125]),
];

/// Longest prefix first: "claude-opus-5" also prefixes "claude-opus-5-5".
fn price<const N: usize>(table: &[(&str, [f64; N])], model: &str) -> Option<[f64; N]> {
    table.iter().filter(|(p, _)| model.starts_with(p)).max_by_key(|(p, _)| p.len()).map(|&(_, v)| v)
}

/// Tokens and dollars for one local day.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Tally {
    pub tokens: i64,
    pub cost: f64,
}

impl Tally {
    fn add(&mut self, o: Tally) {
        self.tokens += o.tokens;
        self.cost += o.cost;
    }
}

/// Local date (YYYY-MM-DD) -> totals.
type Days = BTreeMap<String, Tally>;

fn num(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        _ => 0,
    }
}

/// An ISO-8601 timestamp or epoch seconds -> local date.
fn local_day(ts: Option<&Value>) -> Option<String> {
    let day = match ts? {
        Value::Number(n) => Local.timestamp_opt(n.as_f64()? as i64, 0).single()?.date_naive(),
        Value::String(s) => match DateTime::parse_from_rfc3339(s) {
            Ok(dt) => dt.with_timezone(&Local).date_naive(),
            Err(_) => NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()?.date(),
        },
        _ => return None,
    };
    Some(day.format("%Y-%m-%d").to_string())
}

fn lines(path: &Path) -> std::io::Result<impl Iterator<Item = Vec<u8>>> {
    let f = BufReader::new(fs::File::open(path)?);
    Ok(f.split(b'\n').map_while(Result::ok))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

/// One API response is written as several lines, one per content block, and
/// each line repeats that response's usage, partially filled in on the early
/// lines while the response is still streaming. So collapse by (message id,
/// request id) and keep the line with the most output tokens, which is the
/// final one. De-duplication is per file.
fn parse_claude(path: &Path) -> std::io::Result<Days> {
    type Key = (Option<String>, Option<String>); // (message id, request id)
    let mut best: HashMap<Key, (Value, String, Value)> = HashMap::new();
    let mut order = Vec::new();
    for line in lines(path)? {
        if !contains(&line, b"\"usage\"") || !contains(&line, b"\"assistant\"") {
            continue;
        }
        let Ok(d) = serde_json::from_slice::<Value>(&line) else { continue };
        if d.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let msg = d.get("message");
        let usage = msg.and_then(|m| m.get("usage"));
        let Some(usage) = usage.filter(|u| u.as_object().is_some_and(|o| !o.is_empty())) else { continue };
        let ts = d.get("timestamp");
        if ts.is_none_or(|t| t.is_null() || t.as_str() == Some("")) {
            continue;
        }
        let model = msg.and_then(|m| m.get("model")).and_then(Value::as_str).unwrap_or("");
        if model.is_empty() || model.starts_with('<') {
            continue; // <synthetic>: no API call happened
        }
        let id = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
        let key = (id(msg.and_then(|m| m.get("id"))), id(d.get("requestId")));
        let out = num(usage.get("output_tokens"));
        match best.get(&key) {
            Some(prev) if out <= num(prev.0.get("output_tokens")) => {}
            prev => {
                if prev.is_none() {
                    order.push(key.clone());
                }
                best.insert(key, (usage.clone(), model.to_owned(), ts.unwrap().clone()));
            }
        }
    }
    let mut days = Days::new();
    for key in order {
        let (usage, model, ts) = &best[&key];
        let inp = num(usage.get("input_tokens"));
        let read = num(usage.get("cache_read_input_tokens"));
        let out = num(usage.get("output_tokens"));
        let creation = usage.get("cache_creation");
        let mut w5 = num(creation.and_then(|c| c.get("ephemeral_5m_input_tokens")));
        let w1h = num(creation.and_then(|c| c.get("ephemeral_1h_input_tokens")));
        if w5 == 0 && w1h == 0 {
            w5 = num(usage.get("cache_creation_input_tokens"));
        }
        let mut t = Tally { tokens: inp + w5 + w1h + read + out, cost: 0.0 };
        if let Some([pin, pout, pread]) = price(CLAUDE_PRICES, model) {
            t.cost = (inp as f64 * pin
                + w5 as f64 * pin * 1.25
                + w1h as f64 * pin * 2.0
                + read as f64 * pread
                + out as f64 * pout)
                / M;
        }
        let Some(day) = local_day(Some(ts)) else { continue };
        days.entry(day).or_default().add(t);
    }
    Ok(days)
}

fn charge(sink: &mut Days, day: Option<String>, model: &str, inp: i64, cached: i64, written: i64, out: i64) {
    let Some(day) = day else { return };
    let uncached = (inp - cached - written).max(0);
    let mut t = Tally { tokens: uncached + cached + written + out, cost: 0.0 };
    if let Some([pin, pout, pcached, pwrite]) = price(CODEX_PRICES, model) {
        t.cost = (uncached as f64 * pin + cached as f64 * pcached + written as f64 * pwrite + out as f64 * pout) / M;
    }
    sink.entry(day).or_default().add(t);
}

/// Newer Codex writes one `token_usage_record` per API response. Older
/// versions wrote `token_count` events carrying a running total for the
/// session, so those are differenced. A current rollout contains *both* kinds
/// for the same responses, so the two are accumulated separately and the
/// modern one wins; adding them double-counts every token.
fn parse_codex(path: &Path) -> std::io::Result<Days> {
    let (mut modern, mut legacy) = (Days::new(), Days::new());
    let mut model = "unknown".to_string();
    let mut running: Option<[i64; 4]> = None;
    let mut seen = std::collections::HashSet::new();
    for line in lines(path)? {
        if !contains(&line, b"\"model\"") && !contains(&line, b"token") {
            continue;
        }
        let Ok(d) = serde_json::from_slice::<Value>(&line) else { continue };
        let kind = d.get("type").and_then(Value::as_str).unwrap_or("");
        let p = d.get("payload").unwrap_or(&Value::Null);
        if kind == "turn_context" || kind == "session_meta" {
            let m = p.get("model").filter(|m| truthy(m)).or_else(|| p.get("payload").and_then(|q| q.get("model")));
            if let Some(m) = m.and_then(Value::as_str).filter(|m| !m.is_empty()) {
                model = m.to_string();
            }
            continue;
        }
        let ts = d.get("timestamp");
        if kind == "token_usage_record" {
            let u = p.get("usage").unwrap_or(&Value::Null);
            if let Some(rid) = p.get("response_id").and_then(Value::as_str).filter(|r| !r.is_empty())
                && !seen.insert(rid.to_string())
            {
                continue;
            }
            let f = |k| num(u.get(k));
            charge(
                &mut modern,
                local_day(ts),
                &model,
                f("input_tokens"),
                f("cached_input_tokens"),
                f("cache_write_input_tokens"),
                f("output_tokens"),
            );
        } else if kind == "event_msg" && p.get("type").and_then(Value::as_str) == Some("token_count") {
            let Some(info) = p.get("info").filter(|i| truthy(i)) else { continue };
            let tot = info.get("total_token_usage").unwrap_or(&Value::Null);
            let cur = ["input_tokens", "cached_input_tokens", "cache_write_input_tokens", "output_tokens"]
                .map(|k| num(tot.get(k)));
            let delta = match running {
                Some(r) if cur[0] >= r[0] => [cur[0] - r[0], cur[1] - r[1], cur[2] - r[2], cur[3] - r[3]],
                _ => cur, // first record, or the session restarted
            };
            running = Some(cur);
            if delta.iter().any(|&v| v != 0) {
                charge(&mut legacy, local_day(ts), &model, delta[0], delta[1], delta[2], delta[3]);
            }
        }
    }
    Ok(if modern.is_empty() { legacy } else { modern })
}

pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/* ---- collection ---------------------------------------------------------- */

pub fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}

pub fn codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Every *.jsonl under dir, like glob's `**/*.jsonl`: hidden names skipped.
fn jsonl_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            jsonl_under(&path, out);
        } else if name.ends_with(".jsonl") {
            out.push(path);
        }
    }
}

struct Entry {
    stamp: String,
    days: Days,
}

/// Re-counts the month on demand, re-reading only transcripts that changed.
pub struct Counter {
    cache: HashMap<PathBuf, Entry>,
    cache_path: PathBuf,
}

/// This calendar month (local time), Claude and Codex summed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonthUsage {
    pub tokens: i64,
    pub usd: f64,
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl Counter {
    pub fn new() -> Self {
        let dir = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| {
            if cfg!(target_os = "macos") { home().join("Library/Caches") } else { home().join(".cache") }
        });
        let cache_path = dir.join("token-cafe").join("usage.json");
        let cache = Self::load(&cache_path).unwrap_or_default();
        Counter { cache, cache_path }
    }

    fn load(path: &Path) -> Option<HashMap<PathBuf, Entry>> {
        let v: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        if v.get("version")?.as_u64()? != CACHE_VERSION {
            return None;
        }
        let mut out = HashMap::new();
        for (path, e) in v.get("files")?.as_object()? {
            let mut days = Days::new();
            for (day, t) in e.get("days")?.as_object()? {
                days.insert(day.clone(), Tally { tokens: t.get(0)?.as_i64()?, cost: t.get(1)?.as_f64()? });
            }
            out.insert(PathBuf::from(path), Entry { stamp: e.get("stamp")?.as_str()?.to_string(), days });
        }
        Some(out)
    }

    fn save(&self) {
        let files: serde_json::Map<String, Value> = self
            .cache
            .iter()
            .map(|(p, e)| {
                let days: serde_json::Map<String, Value> =
                    e.days.iter().map(|(d, t)| (d.clone(), json!([t.tokens, t.cost]))).collect();
                (p.to_string_lossy().into_owned(), json!({ "stamp": e.stamp, "days": days }))
            })
            .collect();
        let doc = json!({ "version": CACHE_VERSION, "files": files });
        let tmp = self.cache_path.with_extension("json.tmp");
        let ok = self.cache_path.parent().is_some_and(|d| fs::create_dir_all(d).is_ok())
            && fs::write(&tmp, doc.to_string()).is_ok();
        if ok {
            let _ = fs::rename(&tmp, &self.cache_path);
        }
    }

    /// Totals per local day over every transcript written since `since`
    /// (epoch seconds): an older file holds nothing newer than its mtime.
    /// Returns the days and how many files were re-read.
    pub fn collect(&mut self, since: f64) -> (Days, usize) {
        let (mut claude, mut codex) = (Vec::new(), Vec::new());
        jsonl_under(&claude_dir().join("projects"), &mut claude);
        jsonl_under(&codex_dir().join("sessions"), &mut codex);
        let mut fresh = HashMap::new();
        let mut totals = Days::new();
        let mut reparsed = 0;
        type Parse = fn(&Path) -> std::io::Result<Days>;
        for (files, parse) in [(claude, parse_claude as Parse), (codex, parse_codex as Parse)] {
            for path in files {
                let Ok(st) = fs::metadata(&path) else { continue };
                let mtime = st.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
                let Some(mtime) = mtime else { continue };
                if mtime.as_secs_f64() < since {
                    continue;
                }
                let stamp = format!("{}:{}", st.len(), mtime.as_nanos());
                let entry = match self.cache.remove(&path) {
                    Some(e) if e.stamp == stamp => e,
                    _ => {
                        reparsed += 1;
                        // Retry transient read failures on the next poll instead of caching zero.
                        let Ok(days) = parse(&path) else { continue };
                        Entry { stamp, days }
                    }
                };
                for (d, t) in &entry.days {
                    totals.entry(d.clone()).or_default().add(*t);
                }
                fresh.insert(path, entry);
            }
        }
        let dropped = !self.cache.is_empty();
        self.cache = fresh;
        if reparsed > 0 || dropped {
            self.save();
        }
        (totals, reparsed)
    }

    /// This calendar month (local time), Claude and Codex summed.
    pub fn month(&mut self) -> MonthUsage {
        let today = Local::now().date_naive();
        let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
        let since = Local
            .from_local_datetime(&first.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map_or(0.0, |t| t.timestamp() as f64);
        let (days, _) = self.collect(since);
        let (lo, hi) = (first.format("%Y-%m-%d").to_string(), today.format("%Y-%m-%d").to_string());
        let mut sum = Tally::default();
        for (_, t) in days.range(lo..=hi) {
            sum.add(*t);
        }
        MonthUsage { tokens: sum.tokens, usd: (sum.cost * 100.0).round() / 100.0 }
    }
}

/// 1.6 B / 53.0 M / 999.0 K, for the log.
pub fn human(n: i64) -> String {
    for (unit, div) in [("B", 1_000_000_000), ("M", 1_000_000), ("K", 1_000)] {
        if n >= div {
            return format!("{:.1} {unit}", n as f64 / div as f64);
        }
    }
    n.to_string()
}
