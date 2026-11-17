use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::ProcInfo;

const ATTACH_JAR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/attach.jar"));
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn processes() -> Vec<ProcInfo> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
    let kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_cwd(UpdateKind::Always);
    let Ok(sys) = System::new_with_specifics(RefreshKind::nothing().with_processes(kind)) else {
        return Vec::new();
    };
    sys.processes()
        .iter()
        .map(|(pid, p)| ProcInfo {
            pid: pid.as_u32() as i32,
            args: p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect(),
            cwd: p.cwd().map(|c| c.display().to_string()).unwrap_or_default(),
        })
        .collect()
}

fn find_java() -> PathBuf {
    std::env::var_os("JAVA_HOME")
        .map(|h| PathBuf::from(h).join("bin").join("java.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("java.exe"))
}

// goes through the standard Attach API with a small helper. that needs a full
// JDK, the runtime bundled with launchers lacks the jdk.attach module. of course it does
pub fn load_agent(pid: i32, jar: &Path, options: &str) -> Result<()> {
    let helper = jar.with_file_name("attach.jar");
    if std::fs::read(&helper).ok().as_deref() != Some(ATTACH_JAR) {
        std::fs::write(&helper, ATTACH_JAR)?;
    }
    let out = Command::new(find_java())
        .arg("-cp")
        .arg(&helper)
        .arg("ghostslot.Attach")
        .arg(pid.to_string())
        .arg(jar)
        .arg(options)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .context("java.exe not found, install JDK 17+ or set JAVA_HOME")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("com.sun.tools.attach") || err.contains("jdk.attach") {
            bail!("auto join needs a full JDK, not a JRE: install JDK 17+ or set JAVA_HOME");
        }
        bail!("attach failed: {}", err.trim());
    }
    Ok(())
}
