use std::{env, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_HASH");
    if Path::new(".git").exists() {
        println!("cargo:rerun-if-changed=.git");
        for reference in ["HEAD", "packed-refs"] {
            watch_git_path(reference);
        }
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch_git_path(&reference);
        }
    }

    let hash = env::var("BUILD_HASH")
        .ok()
        .filter(|value| valid_hash(value))
        .or_else(|| {
            if Path::new(".git").exists() {
                git(&["rev-parse", "--verify", "--short=12", "HEAD"])
                    .filter(|value| valid_hash(value))
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=REOLINK_MQTT_BUILD_HASH={hash}");
}

fn watch_git_path(reference: &str) {
    if let Some(path) = git(&["rev-parse", "--git-path", reference])
        && Path::new(&path).exists()
    {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn valid_hash(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}
