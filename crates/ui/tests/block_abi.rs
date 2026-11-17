// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "macos")]
use std::sync::Arc;

#[test]
fn native_block_copy_invokes_and_releases_captures() {
    let value = Arc::new(41_u64);
    let capture = value.clone();
    let copied = block::ConcreteBlock::new(move |increment: u64| *capture + increment).copy();
    assert_eq!(Arc::strong_count(&value), 2);
    // The copied block and its captured Arc are alive for this native ABI invocation.
    assert_eq!(unsafe { copied.call((1,)) }, 42);
    drop(copied);
    assert_eq!(Arc::strong_count(&value), 1);
}
