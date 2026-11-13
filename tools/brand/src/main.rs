// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Command line entry point of the brand asset builder.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use eagle_brand::build::build_all;

/// Builds the Eagle brand assets from the SVG sources.
#[derive(Parser)]
#[command(name = "eagle-brand", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Writes every asset and the preview sheet.
    Build {
        /// Directory with the SVG sources.
        #[arg(long, default_value = "assets/brand/src")]
        src: PathBuf,
        /// Output directory. It is removed and created again.
        #[arg(long, default_value = "assets/brand/out")]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Build { src, out } => build_all(&src, &out),
    }
}
