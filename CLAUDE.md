# token-cafe

A Rust desktop app that shows the Claude Code and Codex sessions on this machine as patrons of a pixel-art night café. Supports Linux and macOS; Windows process discovery is not implemented.

## Commands

- `cargo build --release`, then `target/release/token-cafe` (live), `--demo` (fixed list showing every pose), `--print` (sessions and month total as text), `--scale N` (initial size, default 3x). Keys: 1-6 window size, f / F11 fullscreen, Esc leave fullscreen, t toggle always on top, q quit. `--always-on-top` starts pinned; combine with `--scale 1` for a small floating café.
- `cargo test`, `cargo clippy --all-targets` (kept warning-free), `cargo fmt` (`rustfmt.toml`: width 120).
- After editing `design/cafe.js`: `node tools/gen_assets.mjs`, then `cargo run --release --example screenshots` to refresh the README images in `docs/`.

## Layout

- `design/cafe.js`: the art (sprites as palette strings) and the scene's behaviour in plain JS. `tools/gen_assets.mjs` turns it into `src/assets.rs` (palette-slot sprites, 0xRRGGBB colours, 5x7 font) and `src/bg.rgb` (the static room). Both are generated: `assets.rs` is skipped by rustfmt (`#[rustfmt::skip]` in `lib.rs`), `bg.rgb` is marked binary in `.gitattributes`.
- `src/scene.rs` + `src/gfx.rs`: the café, drawn into a 320x240 0xRRGGBB frame.
- `src/collect/`: which sessions are live. `procfs.rs` implements Linux discovery; `macos.rs` uses libproc and sysctl on macOS.
- `src/usage.rs`: this month's tokens and list-price dollars from both tools' transcripts.
- `src/main.rs`: CLI and two feeder threads (sessions every 1 s, usage every 60 s) sending `Update`s over a channel. `src/app.rs`: the winit 0.30 + softbuffer 0.4 window.
- `examples/screenshots.rs`: renders `docs/cafe.png`, `docs/night.png` and `docs/arrivals.gif` from the scene (the GIF stores only changed pixels per frame).

## How it behaves

- The scene ticks in fixed 33 ms steps. The first session list seats everyone at once; later newcomers walk in from the door and leavers walk out. 8 seats; the rest wait outside. A patron's looks are an FNV-1a hash of the session id.
- Idle patrons stretch or sip, take coffee after 2 min and fall asleep after 10. `Session::age_s` (time in the current state) keeps that right across an app restart.
- Names are shortened in the scene, 12 characters for the message window and 8 for the nameplate, keeping a `-1c` style suffix of up to 3 characters.
- Claude Code: `~/.claude/sessions/<pid>.json` (status busy/idle/waiting, `waitingFor`: "permission prompt" -> "!", anything else -> "?"). A file counts only while its `procStart` (clock ticks since boot) equals field 22 of `/proc/<pid>/stat`, so dead or reused pids are skipped. On macOS, compare its formatted `procStart` date with the native process start time, falling back to `startedAt` for older files.
- Claude desktop: Code-tab sessions use the same live PID/status records. Join `cliSessionId` to the cached macOS `claude-code-sessions` metadata for titles/archive flags; never treat saved history as liveness. Ordinary Chat and Cowork are not collected.
- Codex CLI: `codex` TUI processes (subcommands like `exec` and `mcp` are not sessions) -> the rollout JSONL they hold open, else the newest `threads` row for their cwd in `~/.codex/state_5.sqlite` -> last `task_started` vs `task_complete`/`turn_aborted`. The first read scans back in 256 KB chunks until a task event turns up; later reads take only what was appended. Codex logs no approval requests, so it never shows "wait".
- Codex desktop: parse global option values before identifying `app-server`; exclude `exec-server` and other helpers. Each loaded root rollout is a separate patron, titled from SQLite and deduplicated against CLI sessions. Exclude archived roots and subagent-only logs. Root task events determine its state; old subagent activity cannot keep a completed root busy. No cwd/database history fallback for servers.
- Usage: list prices are in `usage.rs` (update them when vendors change prices). Transcripts last written before the 1st of the month are skipped; per-file results are cached in `~/.cache/token-cafe/usage.json` on Linux or `~/Library/Caches/token-cafe/usage.json` on macOS (`XDG_CACHE_HOME` overrides either) by size and mtime. `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are honoured.
- Window: the frame is scaled by the largest whole number that fits, letterboxed. Only what changed since the presented buffer was last shown is repainted (softbuffer buffer age + `present_with_damage`). Wayland app_id and X11 class are `token-cafe`.

## Conventions

- Demo data, tests and screenshots use made-up project names; keep real session names out of the repo.
