// SPDX-License-Identifier: GPL-3.0-or-later
//! Apply the desktop's memory policy before libmalloc initializes, without global OS changes.
use std::{ffi::OsString, os::unix::process::CommandExt, process::Command};

const POLICY: [(&str, &str); 5] = [
    ("MallocNanoZone", "1"),
    ("MallocLargeCache", "0"),
    ("MallocAggressiveMadvise", "1"),
    ("MallocSpaceEfficient", "1"),
    ("MallocMaxMagazines", "1"),
];

fn apply_missing(command: &mut Command, inherited: [Option<OsString>; POLICY.len()]) -> bool {
    let mut changed = false;
    for ((key, value), existing) in POLICY.into_iter().zip(inherited) {
        if existing.is_none() {
            command.env(key, value);
            changed = true;
        }
    }
    changed
}

pub(super) fn initialize() -> std::io::Result<()> {
    let inherited = POLICY.map(|(key, _)| std::env::var_os(key));
    if inherited.iter().all(Option::is_some) {
        return Ok(());
    }
    let mut command = Command::new(std::env::current_exe()?);
    command.args(std::env::args_os().skip(1));
    apply_missing(&mut command, inherited);
    // libmalloc reads these before main. exec preserves PID, arguments and caller overrides.
    Err(command.exec())
}

pub(super) fn configuration() -> String {
    POLICY
        .map(|(key, _)| format!("{key}={}", std::env::var(key).unwrap_or_default()))
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_settings_are_preserved_and_complete_policy_does_not_restart() {
        let mut command = Command::new("eagle");
        command.args(["--fixture-root", "/tmp/Музыка с пробелами"]);
        assert!(apply_missing(
            &mut command,
            [Some("0".into()), None, None, None, None]
        ));
        let variables: Vec<_> = command.get_envs().collect();
        assert_eq!(variables.len(), 4);
        assert!(variables.iter().all(|(key, _)| *key != "MallocNanoZone"));
        assert_eq!(
            command.get_args().last().unwrap(),
            "/tmp/Музыка с пробелами"
        );
        let mut configured = Command::new("eagle");
        assert!(!apply_missing(
            &mut configured,
            std::array::from_fn(|_| Some("0".into()))
        ));
        assert_eq!(configured.get_envs().count(), 0);
    }
}
