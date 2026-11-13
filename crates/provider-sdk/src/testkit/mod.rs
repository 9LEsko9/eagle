// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Test kit for provider authors: an in-memory provider and conformance checks.

mod fake;
pub use fake::{FakeProvider, fake_track};
pub mod conformance;
pub mod replay;
