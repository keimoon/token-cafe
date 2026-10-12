//! The little of /proc the collector needs. Linux only.

use std::fs;
use std::path::PathBuf;

/// Field 22 of /proc/<pid>/stat (start time in clock ticks since boot), or
/// None if the process is gone.
pub fn start_ticks(pid: u32) -> Option<String> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    rest.split_whitespace().nth(19).map(str::to_owned)
}

/// Process start as epoch seconds, or 0 if unknown.
pub fn start_epoch(pid: u32) -> f64 {
    let Some(ticks) = start_ticks(pid).and_then(|t| t.parse::<f64>().ok()) else { return 0.0 };
    boot_time() as f64 + ticks / clk_tck()
}

fn boot_time() -> i64 {
    let s = fs::read_to_string("/proc/stat").unwrap_or_default();
    s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()).unwrap_or(0)
}

fn clk_tck() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

pub fn pids() -> Vec<u32> {
    let Ok(rd) = fs::read_dir("/proc") else { return Vec::new() };
    rd.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

pub fn exe(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// argv without argv[0], empty arguments dropped.
pub fn args(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|&b| b == 0)
            .skip(1)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}

/// Where the process's open file descriptors point.
pub fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
    rd.flatten().filter_map(|e| fs::read_link(e.path()).ok()).collect()
}

/// Keep Linux's exact clock-tick identity check.
pub fn matches_session(pid: u32, session: &serde_json::Value) -> bool {
    let Some(ticks) = start_ticks(pid) else { return false };
    matches_ticks(&ticks, session)
}

fn matches_ticks(ticks: &str, session: &serde_json::Value) -> bool {
    match session.get("procStart").filter(|v| super::truthy(v)) {
        None => true,
        Some(serde_json::Value::String(s)) => *s == ticks,
        Some(other) => {
            other.as_u64().zip(ticks.parse::<u64>().ok()).is_some_and(|(recorded, actual)| recorded == actual)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn start_ticks_accept_strings_and_numbers_without_coercing_other_values() {
        assert!(matches_ticks("12345", &json!({"procStart": "12345"})));
        assert!(matches_ticks("12345", &json!({"procStart": 12345})));
        assert!(matches_ticks("12345", &json!({})));
        assert!(!matches_ticks("12345", &json!({"procStart": 12346})));
        assert!(!matches_ticks("12345", &json!({"procStart": "12346"})));
        for value in [json!(true), json!(-12345), json!(12345.0), json!([12345])] {
            assert!(!matches_ticks("12345", &json!({"procStart": value})));
        }
    }
}
