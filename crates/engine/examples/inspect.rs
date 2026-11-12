// SPDX-License-Identifier: GPL-3.0-or-later
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for file in std::env::args().skip(1) {
        let info = eagle_engine::inspect(&eagle_engine::Source::file(&file), None)?;
        println!("{file}: {info:?}");
    }
    Ok(())
}
