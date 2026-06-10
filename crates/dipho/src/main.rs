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
    Ingest { input: String },
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
        Some(Command::Ingest { .. }) => bail!("not implemented yet (milestone: ingest)"),
        Some(Command::Search { .. }) => bail!("not implemented yet (milestone: word search)"),
        Some(Command::Render { .. }) => bail!("not implemented yet (milestone: render)"),
    }
}
