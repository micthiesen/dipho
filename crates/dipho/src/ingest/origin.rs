//! Source identity and the original-bytes stage. `origin_id` is the
//! pre-download idempotency key: for URLs the normalized yt-dlp extractor +
//! video id (checked before any bytes move), for local files the SHA-256 of
//! the original file. Hashing downloaded bytes can't provide idempotency —
//! a re-download of the same video yields different bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub enum Input {
    Url(String),
    File(PathBuf),
}

pub fn classify(input: &str) -> Result<Input> {
    if input.starts_with("http://") || input.starts_with("https://") {
        return Ok(Input::Url(input.to_string()));
    }
    let path = PathBuf::from(input);
    if !path.is_file() {
        bail!("{input} is neither a URL nor an existing file");
    }
    Ok(Input::File(path))
}

pub fn origin_id(input: &Input) -> Result<String> {
    match input {
        Input::File(path) => Ok(format!("sha256:{}", sha256_file(path)?)),
        Input::Url(url) => {
            let out = Command::new("yt-dlp")
                .args([
                    "--no-playlist",
                    "--no-warnings",
                    "--print",
                    "%(extractor_key)s:%(id)s",
                ])
                .arg(url)
                .output()
                .context("running yt-dlp (is it installed?)")?;
            if !out.status.success() {
                bail!(
                    "yt-dlp probe failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let id = String::from_utf8(out.stdout)?.trim().to_lowercase();
            if id.is_empty() || !id.contains(':') {
                bail!("yt-dlp probe returned no extractor:id for {url}");
            }
            Ok(format!("ytdlp:{id}"))
        }
    }
}

/// Filesystem-safe directory name for a workdir key.
pub fn sanitize(origin_id: &str) -> String {
    origin_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Stage `original.bin` into the workdir: download (URL) or copy (file),
/// via a temp name + rename so a partial transfer never looks complete.
pub fn fetch_original(input: &Input, workdir: &Path) -> Result<()> {
    let dest = workdir.join("original.bin");
    match input {
        Input::File(path) => {
            let tmp = workdir.join("original.bin.tmp");
            fs::copy(path, &tmp).with_context(|| format!("copying {}", path.display()))?;
            fs::rename(&tmp, &dest)?;
        }
        Input::Url(url) => {
            let template = workdir.join("download.%(ext)s");
            let status = Command::new("yt-dlp")
                .args([
                    "--no-playlist",
                    "-f",
                    "bv*+ba/b",
                    "--merge-output-format",
                    "mkv",
                    "-o",
                ])
                .arg(&template)
                .arg(url)
                .status()
                .context("running yt-dlp")?;
            if !status.success() {
                bail!("yt-dlp download failed for {url}");
            }
            let mut downloads: Vec<PathBuf> = fs::read_dir(workdir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("download."))
                })
                .collect();
            match downloads.as_slice() {
                [_] => fs::rename(downloads.remove(0), &dest)?,
                other => bail!("expected one downloaded file, found {}", other.len()),
            }
        }
    }
    Ok(())
}
