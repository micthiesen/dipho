mod ingest;
mod mpv;
mod search;
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
    },
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
        Some(Command::Render { .. }) => bail!("not implemented yet (milestone: render)"),
    }
}
