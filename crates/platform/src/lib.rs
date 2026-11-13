// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform services.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod autostart;
pub mod logging;
pub mod power;
