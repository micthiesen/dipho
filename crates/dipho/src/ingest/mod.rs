//! `dipho ingest <url|file>`: staged, resumable workdir keyed by origin_id —
//! original bytes → playback master + analysis wav → sidecar ML stages →
//! in-process load into the corpus. Every stage that already produced its
//! output is skipped on re-run; a workdir without `manifest.json` is
//! incomplete by definition.

mod normalize;
mod origin;
mod probe;
mod sidecar;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dipho_core::corpus::{Corpus, Manifest, SourceMeta, prosody_from_npz};

pub struct Options {
    /// Corpus database path; the workdir tree lives next to it.
    pub corpus_db: PathBuf,
    pub force: bool,
}

pub fn run(input: &str, opts: &Options) -> Result<()> {
    let input = origin::classify(input)?;
    let origin_id = origin::origin_id(&input)?;
    println!("origin_id: {origin_id}");

    let corpus_dir = opts
        .corpus_db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&corpus_dir)?;
    let mut corpus = Corpus::open(&opts.corpus_db)?;
    corpus.migrate()?;
    if !opts.force
        && let Some(id) = corpus.find_source(&origin_id)?
    {
        println!("already ingested as source {id} — use --force to re-ingest");
        return Ok(());
    }

    let workdir = corpus_dir.join("ingest").join(origin::sanitize(&origin_id));
    fs::create_dir_all(&workdir)?;
    let workdir = workdir.canonicalize()?;

    if workdir.join("original.bin").exists() {
        println!("  original     cached");
    } else {
        println!("  fetching original...");
        origin::fetch_original(&input, &workdir)?;
    }

    let master = match normalize::load_master_info(&workdir)? {
        Some(info) => {
            println!("  master       cached");
            info
        }
        None => {
            println!("  normalizing (playback master + analysis wav)...");
            normalize::create_master(&workdir)?
        }
    };

    sidecar::run_sidecar(&workdir)?;

    let manifest_json = fs::read_to_string(workdir.join("manifest.json"))
        .context("reading manifest.json (sidecar should have written it)")?;
    let manifest = Manifest::from_json(&manifest_json)?;
    let npz = fs::read(workdir.join(&manifest.prosody.path))?;
    let prosody = prosody_from_npz(&npz, manifest.prosody.hop)?;

    let meta = SourceMeta {
        origin: match &input {
            origin::Input::Url(url) => url.clone(),
            origin::Input::File(path) => path.display().to_string(),
        },
        origin_id,
        original_path: Some(workdir.join("original.bin").display().to_string()),
        master_path: workdir.join("master.mkv").display().to_string(),
        master_hash: master.master_hash,
        fps: master.fps,
        has_video: master.has_video,
        start_offsets: Some(master.start_offsets),
    };
    let report = corpus.load_source(&meta, &manifest, &prosody)?;
    println!(
        "loaded source {:?}: {} speakers, {} utterances, {} words, {} phones, {} diphones",
        report.source_id,
        report.speakers,
        report.utterances,
        report.words,
        report.phones,
        report.diphones
    );
    Ok(())
}
