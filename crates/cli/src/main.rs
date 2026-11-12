// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(scenario) =
        std::env::args().find_map(|arg| arg.strip_prefix("--bench-library-").map(str::to_owned))
    {
        return eagle_app::benchmark::run(&eagle_app::data_directory()?, &scenario);
    }
    eagle_app::run()
}
