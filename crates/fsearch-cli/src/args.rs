use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Search case-sensitive substrings in full file and directory paths"
)]
pub struct Args {
    /// Starting directory or existing file path
    #[arg(short, long)]
    pub directory: PathBuf,
    /// Literal, case-sensitive substring to find in the entire path
    #[arg(short, long)]
    pub filename: String,
}
