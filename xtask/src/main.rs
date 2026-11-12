// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("bench") {
        eprintln!("Usage: cargo xtask bench [--seconds 60]");
        return ExitCode::FAILURE;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let result = Command::new(if cfg!(windows) { "python" } else { "python3" })
        .arg(root.join("bench/measure.py"))
        .args(&args[1..])
        .current_dir(root)
        .status();
    match result {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        other => {
            eprintln!("Benchmark failed: {other:?}");
            ExitCode::FAILURE
        }
    }
}
