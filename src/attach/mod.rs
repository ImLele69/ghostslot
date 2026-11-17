use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::auth::Account;

#[cfg(unix)]
#[path = "linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

const AGENT_JAR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/agent.jar"));

#[derive(Clone)]
pub struct McClient {
    pub pid: i32,
    pub username: Option<String>,
    pub version: String,
    pub game_dir: String,
    pub fabric: bool,
    attach_blocked: bool,
    account: Option<Account>,
}

impl McClient {
    pub fn label(&self) -> String {
        let dir = Path::new(self.game_dir.trim_end_matches('/'))
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!(
            "{} · {} · {} (pid {})",
            self.username.as_deref().unwrap_or("?"),
            self.version,
            dir,
            self.pid
        )
    }

    // the session token from the launcher command line, when there is one
    // (CurseForge and the vanilla launcher pass it, Prism does not)
    pub fn account(&self) -> Option<&Account> {
        self.account.as_ref()
    }
}

fn arg_after<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn find_clients() -> Vec<McClient> {
    let me = std::process::id() as i32;
    let mut out: Vec<McClient> = platform::processes()
        .into_iter()
        .filter(|p| p.pid != me)
        .filter_map(classify)
        .collect();
    out.sort_by_key(|c| c.pid);
    out
}

pub(crate) struct ProcInfo {
    pub pid: i32,
    pub args: Vec<String>,
    pub cwd: String,
}

fn classify(p: ProcInfo) -> Option<McClient> {
    let ProcInfo { pid, args, cwd } = p;
    let exe = args.first()?.to_ascii_lowercase();
    let exe = exe.trim_end_matches(".exe");
    if !exe.ends_with("java") && !exe.ends_with("javaw") {
        return None;
    }
    let joined = args.join(" ");
    let is_client = joined.contains("net.minecraft.client.main.Main")
        || joined.contains("KnotClient")
        || joined.contains("org.prismlauncher.EntryPoint")
        || (arg_after(&args, "--gameDir").is_some() && arg_after(&args, "--version").is_some());
    if !is_client {
        return None;
    }
    let username = arg_after(&args, "--username").map(str::to_string);
    let account = match (
        username.as_deref(),
        arg_after(&args, "--uuid"),
        arg_after(&args, "--accessToken"),
    ) {
        (Some(name), Some(uuid), Some(token)) if token.len() > 20 => {
            Some(Account::from_client(name, uuid, token))
        }
        _ => None,
    };
    Some(McClient {
        pid,
        username,
        version: arg_after(&args, "--version").unwrap_or("?").to_string(),
        game_dir: arg_after(&args, "--gameDir").map(str::to_string).unwrap_or(cwd),
        fabric: joined.contains("fabric-loader") || joined.contains("KnotClient"),
        attach_blocked: args.iter().any(|a| a == "-Xrs" || a == "-XX:+DisableAttachMechanism"),
        account,
    })
}

fn cache_dir() -> Result<PathBuf> {
    let dir = dirs::cache_dir().context("cache directory")?.join("ghostslot");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
pub(crate) fn check_load_reply(resp: &str) -> Result<()> {
    let mut lines = resp.lines();
    if lines.next() != Some("0") {
        bail!("attach refused: {}", resp.trim());
    }
    let code = lines.next().unwrap_or("0").trim();
    let code = code.strip_prefix("return code:").unwrap_or(code).trim();
    if code != "0" {
        bail!("the agent did not load: {}", resp.trim());
    }
    Ok(())
}

// makes the running client leave its current server and connect to `address`.
// blocks for up to 10s, so keep it off the GUI thread
pub fn join(client: &McClient, address: &str) -> Result<Option<String>> {
    if !client.fabric {
        bail!("auto join only works with Fabric clients for now");
    }
    if client.attach_blocked {
        bail!("this JVM has attach disabled (-Xrs or DisableAttachMechanism)");
    }
    let dir = cache_dir()?;
    // one file per agent build, a running JVM keeps the jars it already opened
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        AGENT_JAR.hash(&mut h);
        h.finish()
    };
    let jar = dir.join(format!("agent-{hash:016x}.jar"));
    if !jar.exists() {
        std::fs::write(&jar, AGENT_JAR)?;
    }
    let result = dir.join(format!("result-{}-{}", client.pid, std::process::id()));
    let _ = std::fs::remove_file(&result);

    platform::load_agent(
        client.pid,
        &jar,
        &format!("{}|{address}|{}", jar.display(), result.display()),
    )?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&result) {
            if !text.is_empty() {
                let _ = std::fs::remove_file(&result);
                if text == "ok" {
                    return Ok(None);
                }
                if let Some(note) = text.strip_prefix("ok:") {
                    return Ok(Some(note.to_string()));
                }
                bail!("{text}");
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("the client did not answer (game paused or frozen?)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn list_running_clients() {
        for c in find_clients() {
            println!("{} fabric={} token={}", c.label(), c.fabric, c.account.is_some());
        }
    }

    // needs a JDK, run with `cargo test -- --ignored`. the dummy JVM has no
    // Minecraft classes, so the agent has to load and report exactly that
    #[test]
    #[ignore]
    fn agent_loads_into_a_live_jvm() {
        let java = std::env::var("ATTACH_TEST_JAVA").unwrap_or_else(|_| "java".into());
        let dir = cache_dir().unwrap().join("attach-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Idle.java"),
            "public class Idle { public static void main(String[] a) throws Exception { Thread.sleep(60000); } }",
        )
        .unwrap();
        let ok = std::process::Command::new("javac")
            .current_dir(&dir)
            .args(["--release", "17", "Idle.java"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "javac");
        let mut child = std::process::Command::new(&java)
            .current_dir(&dir)
            .args(["-cp", ".", "Idle"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let client = McClient {
            pid: child.id() as i32,
            username: None,
            version: "test".into(),
            game_dir: dir.display().to_string(),
            fabric: true,
            attach_blocked: false,
            account: None,
        };
        let err = join(&client, "127.0.0.1:1").unwrap_err().to_string();
        child.kill().unwrap();
        assert!(err.contains("class_310"), "{err}");
    }
}
