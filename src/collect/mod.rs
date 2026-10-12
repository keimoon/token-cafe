//! Which Claude Code and Codex sessions run on this machine and what each is
//! doing.

mod claude;
mod codex;
mod info;
pub use info::SessionInfo;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod process;
#[cfg(not(target_os = "macos"))]
#[path = "procfs.rs"]
mod process;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::scene::{ForcedPose, Kind, Session, SessionDetail, State, Why};
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
    session_id: String,
    project: String,
    transcript: Option<std::path::PathBuf>,
    target: Option<crate::scene::SessionTarget>,
}

#[derive(Default)]
pub struct Collector {
    claude: claude::Claude,
    codex: codex::Codex,
    info: info::InfoCache,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every live session, first come first seated.
    fn found(&mut self) -> Vec<Found> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let mut all = self.claude.sessions(now);
        all.extend(self.codex.sessions(now));
        // Full IDs avoid collisions between UUID prefixes and between providers.
        for f in &mut all {
            f.id = session_key(f.kind, if f.session_id.is_empty() { &f.id } else { &f.session_id });
        }
        all.sort_by(|a, b| a.started.total_cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
        all
    }

    /// Terminal prototype: transcript details for confirmed live sessions only.
    pub fn session_info(&mut self) -> Vec<SessionInfo> {
        let found = self.found();
        self.info.collect(&found)
    }

    /// One discovery snapshot supplies both patrons and their transcript details.
    pub fn sessions_with_details(&mut self) -> (Vec<Session>, Vec<SessionDetail>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let found = self.found();
        let info = self.info.collect(&found);
        let details = found
            .iter()
            .zip(info.iter())
            .map(|(f, i)| {
                let mut detail = i.display(&f.id);
                detail.target = f.target.clone();
                detail
            })
            .collect();
        let sessions = found
            .into_iter()
            .map(|f| Session {
                id: f.id,
                name: f.name,
                kind: f.kind,
                state: f.state,
                why: f.why,
                pose: ForcedPose::Auto,
                age_s: (now - f.since).max(0.0) as u32,
            })
            .collect();
        (sessions, details)
    }

    pub fn sessions(&mut self) -> Vec<Session> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        self.found()
            .into_iter()
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

fn session_key(kind: Kind, id: &str) -> String {
    let provider = if kind == Kind::Codex { "codex" } else { "claude" };
    format!("{provider}:{id}")
}

#[test]
fn session_identity_uses_full_id_and_provider() {
    assert_ne!(session_key(Kind::Claude, "019a1234-aaaa"), session_key(Kind::Claude, "019a1234-bbbb"));
    assert_ne!(session_key(Kind::Codex, "aaaa-12345678"), session_key(Kind::Codex, "bbbb-12345678"));
    assert_ne!(session_key(Kind::Claude, "same"), session_key(Kind::Codex, "same"));
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

// Compile the portable Linux identity regression tests on macOS too.
#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code)]
#[path = "procfs.rs"]
mod linux_identity_tests;
