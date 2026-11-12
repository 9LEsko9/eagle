// SPDX-License-Identifier: GPL-3.0-or-later
//! Prepared, provider-independent inputs. Opening and reading happen on the decoder thread.
use std::{
    fs::File,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};
use symphonia::core::io::MediaSource;

pub type SourceError = Box<dyn std::error::Error + Send + Sync>;
pub type Reader = Box<dyn MediaSource>;
type Open = dyn Fn(Interrupt) -> Result<Reader, SourceError> + Send + Sync;

#[derive(Clone)]
pub struct Interrupt {
    pub generation: Arc<AtomicU32>,
    pub expected: u32,
    pub preload: bool,
    pub seeking: Arc<AtomicBool>,
}
impl Interrupt {
    pub fn cancelled(&self) -> bool {
        self.generation.load(Ordering::Acquire) != self.expected
    }
}

#[derive(Clone)]
pub struct Source(Arc<Open>);
impl Source {
    pub fn new(
        open: impl Fn(Interrupt) -> Result<Reader, SourceError> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(open))
    }
    pub fn file(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_owned();
        Self::new(move |_| Ok(Box::new(File::open(&path)?)))
    }
    pub(crate) fn open(&self, interrupt: Interrupt) -> Result<Reader, SourceError> {
        (self.0)(interrupt)
    }
    pub fn open_reader(&self, interrupt: Interrupt) -> Result<Reader, SourceError> {
        self.open(interrupt)
    }
}
