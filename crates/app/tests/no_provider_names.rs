// SPDX-License-Identifier: GPL-3.0-or-later
//! The app, store and ui know providers only through the SDK: no concrete provider name appears in
//! production code. Tests, examples and `providers::builtin` (the one registration point) may.
use std::{fs, path::Path};

const NAMES: [&str; 2] = ["yandex", "vk"];
/// (file suffix, line fragment) that is allowed on purpose.
const ALLOWED: [(&str, &str); 3] = [
    // The single place where compiled-in providers are listed.
    ("app/src/providers.rs", "eagle_yandex::Yandex"),
    // v9 migration moves the legacy per-provider settings keys of the only provider that had them.
    ("store/src/lib.rs", "account_uid:yandex"),
    ("store/src/lib.rs", "account:yandex"),
];

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "slint") {
            out.push(path);
        }
    }
}

/// Code before the first top-level `#[cfg(test)]`, without comment lines.
fn production(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .take_while(|(_, l)| *l != "#[cfg(test)]")
        .filter(|(_, l)| {
            let l = l.trim_start();
            !(l.starts_with("//") || l.starts_with('*') || l.starts_with("/*"))
        })
}

#[test]
fn production_code_names_no_provider() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = vec![];
    for krate in ["app", "store", "ui"] {
        sources(&crates.join(krate).join("src"), &mut files);
        sources(&crates.join(krate).join("ui"), &mut files);
    }
    assert!(!files.is_empty());
    let mut found = vec![];
    for file in files {
        let path = file.to_string_lossy().replace('\\', "/");
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (n, line) in production(&text) {
            let lower = line.to_lowercase();
            if NAMES.iter().any(|name| {
                lower.contains(&format!("\"{name}"))
                    || lower.contains(&format!("{name}:"))
                    || lower.contains(&format!("eagle_{name}"))
            }) && !ALLOWED
                .iter()
                .any(|(suffix, part)| path.ends_with(suffix) && line.contains(part))
            {
                found.push(format!("{path}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "provider names in production code:\n{}",
        found.join("\n")
    );
}
