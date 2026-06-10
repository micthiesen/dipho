//! Sidecar driver: spawns `uv run dipho-ingest` on the staged workdir and
//! relays its NDJSON progress (contract: python/README.md). Stage skipping
//! on resume is the sidecar's job; ours is to surface progress and turn the
//! terminal error/done record into a Result.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The uv project directory holding `dipho-ingest`. Resolution order:
/// `DIPHO_SIDECAR_DIR`, `./python` (running from a checkout), then the
/// build-time repo path (development binary).
fn sidecar_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("DIPHO_SIDECAR_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let cwd = PathBuf::from("python");
    if cwd.join("pyproject.toml").is_file() {
        return Ok(cwd);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python");
    if repo.join("pyproject.toml").is_file() {
        return Ok(repo);
    }
    bail!("cannot find the dipho-ingest sidecar — set DIPHO_SIDECAR_DIR to its uv project dir");
}

pub fn run_sidecar(workdir: &Path) -> Result<()> {
    let log_path = workdir.join("ingest.log");
    let log = fs::File::create(&log_path)?;
    let mut child = Command::new("uv")
        .arg("run")
        .arg("--project")
        .arg(sidecar_dir()?)
        .args(["dipho-ingest", "--workdir"])
        .arg(workdir)
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .context("spawning the ingest sidecar (is uv installed?)")?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut done = false;
    let mut error: Option<String> = None;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // not ours — stray library output
        };
        if msg["done"].as_bool() == Some(true) {
            done = true;
        } else if let Some(err) = msg["error"].as_object() {
            error = Some(format!(
                "{}: {}",
                err.get("stage").and_then(Value::as_str).unwrap_or("?"),
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error"),
            ));
        } else if let (Some(stage), Some(pct)) = (msg["stage"].as_str(), msg["pct"].as_i64()) {
            let skipped = if msg["skipped"].as_bool() == Some(true) {
                " (cached)"
            } else {
                ""
            };
            println!("  {stage:<12} {pct:>3}%{skipped}");
        }
    }
    let status = child.wait()?;
    if let Some(error) = error {
        bail!(
            "sidecar stage failed — {error}\n  full log: {}",
            log_path.display()
        );
    }
    if !status.success() || !done {
        bail!(
            "sidecar exited without completing (status {status})\n  full log: {}",
            log_path.display()
        );
    }
    Ok(())
}
