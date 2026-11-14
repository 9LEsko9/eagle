// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Test kit for provider authors: an in-memory provider and conformance checks.

mod fake;
pub use fake::{
    FAKE_SECOND_FACTOR, FAKE_SIGN_IN_TOKEN, FAKE_WRONG_PASSWORD, FakeProvider, fake_track,
};
pub mod conformance;
pub mod replay;
