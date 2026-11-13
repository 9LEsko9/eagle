// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Characterization: provider boundary types stay reachable through `eagle_core`.

#[test]
fn track_id_parses_through_core_path() {
    let id: eagle_core::TrackId = "yandex:1".parse().expect("valid track id");
    assert_eq!(id.provider(), "yandex");
    assert_eq!(id.local(), "1");
}

#[test]
fn token_display_is_redacted_through_core_path() {
    let token = eagle_core::Token::new("s".into());
    assert_eq!(format!("{token}"), "[REDACTED]");
}

#[test]
fn signed_url_debug_is_redacted_through_core_path() {
    let url = eagle_core::SignedUrl::new("https://x".into());
    assert_eq!(format!("{url:?}"), "[REDACTED URL]");
}
