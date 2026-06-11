mod ingest;
mod mpv;
mod render;
mod search;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "dipho",
    version,
    about = "A TUI for making YTPs and sentence mixes"
)]
struct Cli {
    /// Corpus database
    #[arg(long, global = true, default_value = ".dipho/corpus.db")]
    corpus: std::path::PathBuf,

    /// Edit file the TUI works on (created on first save; recovered from
    /// its .autosave if a session ended without saving)
    #[arg(long, default_value = "edit.json")]
    edit: std::path::PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest a source (URL or local file) into the corpus
    Ingest {
        input: String,
        /// Re-ingest even if this origin_id is already in the corpus
        #[arg(long)]
        force: bool,
    },
    /// Search the corpus for a word or phrase
    Search { query: String },
    /// Render an edit to a file via ffmpeg
    Render {
        edit: std::path::PathBuf,
        output: std::path::PathBuf,
        /// Output resolution, e.g. 1920x1080 (default: the largest
        /// source's)
        #[arg(long, value_parser = parse_size)]
        size: Option<(u32, u32)>,
        /// Output frame rate (default: the largest source's)
        #[arg(long)]
        fps: Option<f64>,
        /// Persist an origin_id-only rebind back into the edit file
        #[arg(long)]
        accept_relink: bool,
    },
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{s:?} is not WxH"))?;
    match (w.parse(), h.parse()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(format!("{s:?} is not WxH")),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(cli.corpus, cli.edit),
        Some(Command::Ingest { input, force }) => ingest::run(
            &input,
            &ingest::Options {
                corpus_db: cli.corpus,
                force,
            },
        ),
        Some(Command::Search { query }) => search::run(&query, &cli.corpus),
        Some(Command::Render {
            edit,
            output,
            size,
            fps,
            accept_relink,
        }) => render::run(
            &edit,
            &output,
            &render::Options {
                corpus_db: cli.corpus,
                size,
                fps,
                accept_relink,
            },
        ),
    }
}
