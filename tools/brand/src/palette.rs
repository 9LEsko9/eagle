// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Brand colors.

/// Dark background of the app icon.
pub const GRAPHITE: &str = "#1A1A1D";
/// Light disc used on the icon.
pub const DISC: &str = "#F2F2F4";
/// Accent color of the icon.
pub const GOLD: &str = "#E9A23B";

/// Parses a `#RRGGBB` color into its channels.
///
/// Returns `[0, 0, 0]` when `hex` is not a six-digit hex color. Brand colors
/// are constants, so a malformed input is a programming error and is not
/// reported as a `Result`.
pub fn rgb(hex: &str) -> [u8; 3] {
    let Some(digits) = hex.strip_prefix('#') else {
        return [0, 0, 0];
    };
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return [0, 0, 0];
    }
    // All six bytes are ASCII hex digits, so the slices below are valid and parse.
    let channel = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).unwrap_or(0);
    [channel(0), channel(2), channel(4)]
}
