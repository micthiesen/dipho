mod ingest;
mod mpv;
mod tui;

use anyhow::bail;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "dipho",
    version,
    about = "A TUI for making YTPs and sentence mixes"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest a source (URL or local file) into the corpus
    Ingest {
        input: String,
        /// Corpus database (default: ./.dipho/corpus.db)
        #[arg(long)]
        corpus: Option<std::path::PathBuf>,
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
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(),
        Some(Command::Ingest {
            input,
            corpus,
            force,
        }) => ingest::run(
            &input,
            &ingest::Options {
                corpus_db: corpus.unwrap_or_else(|| ".dipho/corpus.db".into()),
                force,
            },
        ),
        Some(Command::Search { .. }) => bail!("not implemented yet (milestone: word search)"),
        Some(Command::Render { .. }) => bail!("not implemented yet (milestone: render)"),
    }
}
