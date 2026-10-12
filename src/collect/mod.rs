//! Which Claude Code and Codex sessions run on this machine and what each is
//! doing.

mod claude;
mod codex;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod process;
#[cfg(not(target_os = "macos"))]
#[path = "procfs.rs"]
mod process;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::scene::{ForcedPose, Kind, Session, State, Why};
pub(crate) use crate::usage::truthy;

/// One live session, as found on disk.
struct Found {
    id: String,
    name: String,
    kind: Kind,
    state: State,
    why: Why,
    /// Epoch seconds the session entered this state.
    since: f64,
    /// Epoch seconds the session started, 0 if unknown.
    started: f64,
}

#[derive(Default)]
pub struct Collector {
    claude: claude::Claude,
    codex: codex::Codex,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every live session, first come first seated.
    pub fn sessions(&mut self) -> Vec<Session> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let mut all = self.claude.sessions(now);
        all.extend(self.codex.sessions(now));
        all.sort_by(|a, b| a.started.total_cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
        all.into_iter()
            .map(|f| Session {
                id: f.id,
                name: f.name,
                kind: f.kind,
                state: f.state,
                why: f.why,
                pose: ForcedPose::Auto,
                age_s: (now - f.since).max(0.0) as u32,
            })
            .collect()
    }
}

/// The last path component ('' after a trailing '/').
fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

#[cfg(test)]
struct TestDir(std::path::PathBuf);

#[cfg(test)]
impl TestDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("token-cafe-{}-{unique}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
