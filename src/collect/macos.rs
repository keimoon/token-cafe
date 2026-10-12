//! Darwin process discovery through libproc and sysctl. No subprocesses are
//! spawned during polling; inaccessible or exited processes are skipped.

use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use chrono::{Local, NaiveDateTime, TimeZone};
use serde_json::Value;

fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let pid = i32::try_from(pid).ok().filter(|&p| p > 0)?;
    // SAFETY: this C struct contains only integers/arrays, all valid when zeroed.
    let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: info is a writable buffer of exactly the size requested by this flavor.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, std::ptr::from_mut(&mut info).cast(), size) };
    (read == size).then_some(info)
}

pub fn start_epoch(pid: u32) -> f64 {
    bsd_info(pid).map_or(0.0, |i| i.pbi_start_tvsec as f64 + i.pbi_start_tvusec as f64 / 1e6)
}

/// Claude on macOS writes a UTC `ps -o lstart` date, rather than Linux clock ticks.
/// Prefer that identity; for older files, the process must start before the session.
pub fn matches_session(pid: u32, session: &Value) -> bool {
    let start = start_epoch(pid);
    matches_start(start, session)
}

fn matches_start(start: f64, session: &Value) -> bool {
    if start <= 0.0 {
        return false;
    }
    // A reused pid cannot belong to a session created before this process.
    if session.get("startedAt").and_then(Value::as_f64).is_some_and(|ms| start > ms / 1000.0) {
        return false;
    }
    if let Some(text) = session.get("procStart").and_then(Value::as_str)
        && let Ok(date) = NaiveDateTime::parse_from_str(text.trim(), "%a %b %e %T %Y")
    {
        if date.and_utc().timestamp() == start as i64 {
            return true;
        }
        // Also accept files produced by versions using the local timezone.
        let local = Local.from_local_datetime(&date);
        return [local.earliest(), local.latest()].into_iter().flatten().any(|d| d.timestamp() == start as i64);
    }
    session.get("startedAt").and_then(Value::as_f64).is_some_and(|ms| ms.is_finite() && start <= ms / 1000.0)
}

pub fn pids() -> Vec<u32> {
    // SAFETY: a null buffer queries the current number of processes.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Leave room for processes created between sizing and reading.
    let mut pids = vec![0i32; count as usize + 128];
    // SAFETY: the buffer is initialized, aligned and its byte length is supplied.
    let read = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * size_of::<i32>()) as i32) };
    pids.truncate(read.max(0) as usize);
    pids.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
}

fn path(bytes: impl IntoIterator<Item = libc::c_char>) -> Option<PathBuf> {
    let bytes: Vec<u8> = bytes.into_iter().take_while(|&c| c != 0).map(|c| c as u8).collect();
    (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes)))
}

pub fn exe(pid: u32) -> Option<PathBuf> {
    let pid = i32::try_from(pid).ok()?;
    let mut buffer = vec![0; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer is writable for the supplied byte length.
    let read = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if read <= 0 {
        return None;
    }
    path(buffer)
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: the C struct contains only integers and arrays.
    let mut info: libc::proc_vnodepathinfo = unsafe { zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as i32;
    // SAFETY: the buffer matches the requested flavor and size.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, std::ptr::from_mut(&mut info).cast(), size) };
    if read != size {
        return None;
    }
    path(info.pvi_cdir.vip_path.into_iter().flatten())
}

pub fn args(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, i32::try_from(pid).ok()?];
    let mut size = 0;
    // SAFETY: mib and size are valid; a null output requests the required size.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: buffer has size writable bytes; no new value is being set.
    if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) } != 0
    {
        return None;
    }
    buffer.truncate(size);
    parse_args(&buffer)
}

fn parse_args(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    // KERN_PROCARGS2: argc, executable path, NUL padding, argv, environment.
    let rest = buffer.get(4..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[end..];
    let first = rest.iter().position(|&b| b != 0)?;
    let mut rest = &rest[first..];
    let mut args = Vec::new();
    for i in 0..argc {
        let end = rest.iter().position(|&b| b == 0)?;
        if i > 0 && end > 0 {
            args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        }
        rest = &rest[end + 1..];
    }
    Some(args)
}

// These two structures are declared in Darwin's <sys/proc_info.h> but not libc.
#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

#[repr(C)]
struct VnodeFdInfoWithPath {
    file: ProcFileInfo,
    vnode: libc::vnode_info_path,
}

pub fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(pid) = i32::try_from(pid) else { return Vec::new() };
    // SAFETY: a null buffer requests the descriptor list's byte size.
    let size = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let count = size as usize / size_of::<libc::proc_fdinfo>() + 32;
    // SAFETY: proc_fdinfo contains only integers.
    let mut fds: Vec<libc::proc_fdinfo> = (0..count).map(|_| unsafe { zeroed() }).collect();
    // SAFETY: fds is an aligned writable buffer with the supplied byte length.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (count * size_of::<libc::proc_fdinfo>()) as i32,
        )
    };
    fds.truncate(read.max(0) as usize / size_of::<libc::proc_fdinfo>());
    fds.into_iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(|fd| {
            // SAFETY: the C struct contains only integers and arrays.
            let mut info: VnodeFdInfoWithPath = unsafe { zeroed() };
            let size = size_of::<VnodeFdInfoWithPath>() as i32;
            // PROC_PIDFDVNODEPATHINFO = 2, from <sys/proc_info.h>.
            // SAFETY: info matches this flavor's C layout and supplied size.
            let read = unsafe { libc::proc_pidfdinfo(pid, fd.proc_fd, 2, std::ptr::from_mut(&mut info).cast(), size) };
            if read != size {
                return None;
            }
            path(info.vnode.vip_path.into_iter().flatten())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_boundaries_exclude_environment() {
        let mut raw = 4i32.to_ne_bytes().to_vec();
        raw.extend_from_slice(b"/a path/codex\0\0codex\0--cd\0/a project\0\0SECRET=value\0");
        assert_eq!(parse_args(&raw).unwrap(), ["--cd", "/a project"]);
        assert!(parse_args(&raw[..20]).is_none());
        assert!(parse_args(&[]).is_none());
    }

    #[test]
    fn rejects_dead_and_reused_processes() {
        let start = 1_790_000_000.25;
        let text = Local.timestamp_opt(start as i64, 0).unwrap().format("%a %b %e %T %Y").to_string();
        let session = serde_json::json!({"procStart": text, "startedAt": (start + 1.0) * 1000.0});
        assert!(matches_start(start, &session));
        let utc_text = chrono::DateTime::from_timestamp(start as i64, 0).unwrap().format("%a %b %e %T %Y").to_string();
        assert!(matches_start(start, &serde_json::json!({"procStart": utc_text, "startedAt": (start + 1.0) * 1000.0})));
        assert!(!matches_start(0.0, &session));
        assert!(!matches_start(start + 10.0, &session));
        let old = serde_json::json!({"startedAt": (start + 1.0) * 1000.0});
        assert!(matches_start(start, &old));
        assert!(!matches_start(start + 10.0, &old));
        assert!(!matches_start(start, &Value::Null));
    }

    #[test]
    fn discovers_this_process_and_its_open_file() {
        let pid = std::process::id();
        assert!(pids().contains(&pid));
        assert_eq!(exe(pid).unwrap(), std::env::current_exe().unwrap());
        assert_eq!(cwd(pid).unwrap(), std::env::current_dir().unwrap().canonicalize().unwrap());
        assert!(start_epoch(pid) > 0.0);
        assert_eq!(args(pid).unwrap(), std::env::args().skip(1).filter(|s| !s.is_empty()).collect::<Vec<_>>());
        let file_path = std::env::current_exe().unwrap();
        let _file = std::fs::File::open(&file_path).unwrap();
        assert!(open_files(pid).contains(&file_path));
        assert_eq!(start_epoch(u32::MAX), 0.0);
    }
}
