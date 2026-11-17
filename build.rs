use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("{what}: a JDK is required ({e})"));
    assert!(status.success(), "{what} failed");
}

fn make_jar(out: &Path, name: &str, manifest: &str, sources: &[&str]) {
    let classes = out.join(format!("{name}-classes"));
    let _ = std::fs::remove_dir_all(&classes);
    std::fs::create_dir_all(&classes).unwrap();
    run(
        Command::new("javac").args(["--release", "17", "-d"]).arg(&classes).args(sources),
        "javac",
    );
    let mf = out.join(format!("{name}.MF"));
    std::fs::write(&mf, manifest).unwrap();
    run(
        Command::new("jar")
            .arg("cfm")
            .arg(out.join(format!("{name}.jar")))
            .arg(&mf)
            .arg("-C")
            .arg(&classes)
            .arg("."),
        "jar",
    );
}

fn main() {
    println!("cargo:rerun-if-changed=agent");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    make_jar(
        &out,
        "agent",
        "Manifest-Version: 1.0\nAgent-Class: ghostslot.Loader\n",
        &["agent/ghostslot/Agent.java", "agent/ghostslot/Loader.java"],
    );
    make_jar(
        &out,
        "attach",
        "Manifest-Version: 1.0\nMain-Class: ghostslot.Attach\n",
        &["agent/ghostslot/Attach.java"],
    );
}
