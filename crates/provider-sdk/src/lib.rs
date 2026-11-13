// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provider boundary types shared by eagle crates.

mod secrets;
pub use secrets::{SignedUrl, Token};
mod models;
pub use models::*;
mod info;
pub use info::*;
mod http;
pub use http::*;
