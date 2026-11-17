use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::{check_load_reply, ProcInfo};

pub fn processes() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return out };
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let args = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(ProcInfo { pid, args, cwd });
    }
    out
}

fn socket_path(pid: i32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/root/tmp/.java_pid{pid}"))
}

// HotSpot starts its attach listener when it gets SIGQUIT and finds an
// .attach_pid file. without that file SIGQUIT only prints a thread dump
fn open_attach_socket(pid: i32) -> Result<UnixStream> {
    let sock = socket_path(pid);
    if !sock.exists() {
        let candidates = [
            PathBuf::from(format!("/proc/{pid}/cwd/.attach_pid{pid}")),
            PathBuf::from(format!("/tmp/.attach_pid{pid}")),
        ];
        let trigger = candidates
            .iter()
            .find(|p| std::fs::write(p, b"").is_ok())
            .context("could not create the .attach_pid file")?
            .clone();
        // SAFETY: plain kill(2) on a pid we just read from /proc
        let rc = unsafe { libc::kill(pid, libc::SIGQUIT) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = std::fs::remove_file(&trigger);
        if rc != 0 {
            bail!("kill(SIGQUIT) failed");
        }
        if !sock.exists() {
            bail!("the JVM did not open its attach socket");
        }
    }
    UnixStream::connect(&sock).context("connecting to the attach socket")
}

pub fn load_agent(pid: i32, jar: &Path, options: &str) -> Result<()> {
    let mut s = open_attach_socket(pid)?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut req = Vec::new();
    for part in ["1", "load", "instrument", "false", &format!("{}={options}", jar.display())] {
        req.extend_from_slice(part.as_bytes());
        req.push(0);
    }
    s.write_all(&req)?;
    let mut resp = String::new();
    s.read_to_string(&mut resp)?;
    check_load_reply(&resp)
}
