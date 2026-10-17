#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{Parser, ValueEnum};
use fsearch_core::{IndexLimits, SearchMode};
use std::path::PathBuf;

mod app;
mod model;
mod platform;
mod worker;

#[derive(Parser)]
#[command(version, about = "FSearch desktop file search")]
struct Args {
    /// Preselect and scan this directory (otherwise scanning starts on request)
    #[arg(short, long)]
    directory: Option<PathBuf>,
    /// Initial search text
    #[arg(short, long, default_value = "")]
    query: String,
    /// Initial literal matching mode (always case-sensitive)
    #[arg(long, value_enum, default_value = "name-substring")]
    mode: InitialMode,
    /// Stop indexing at this many entries; stopped builds are explicitly partial
    #[arg(long, default_value_t = IndexLimits::default().max_entries)]
    max_index_entries: usize,
    /// Maximum retained native path bytes per build (not a process RAM limit)
    #[arg(long, default_value_t = IndexLimits::default().max_path_bytes)]
    max_index_path_bytes: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum InitialMode {
    ExactName,
    ExactPath,
    NameSubstring,
    PathSubstring,
}

impl From<InitialMode> for SearchMode {
    fn from(mode: InitialMode) -> Self {
        match mode {
            InitialMode::ExactName => Self::ExactName,
            InitialMode::ExactPath => Self::ExactPath,
            InitialMode::NameSubstring => Self::NameSubstring,
            InitialMode::PathSubstring => Self::PathSubstring,
        }
    }
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size([740.0, 500.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "FSearch",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::SearchApp::new(
                cc,
                args.directory,
                args.query,
                args.mode.into(),
                IndexLimits {
                    max_entries: args.max_index_entries,
                    max_path_bytes: args.max_index_path_bytes,
                    ..IndexLimits::default()
                },
            )?))
        }),
    )
}
