// SPDX-License-Identifier: GPL-3.0-or-later
//! Converts only the queue rows requested by Slint's virtual list.
use crate::QueueItem;
use eagle_core::Track;
use slint::{Model, ModelNotify, ModelTracker};
use std::{cell::Cell, sync::Arc};

pub(crate) struct QueueModel {
    entries: Vec<(u64, Arc<Track>)>,
    current: Cell<Option<u64>>,
    notify: ModelNotify,
}

impl QueueModel {
    pub(crate) fn new(entries: Vec<(u64, Arc<Track>)>, current: Option<u64>) -> Self {
        Self {
            entries,
            current: Cell::new(current),
            notify: ModelNotify::default(),
        }
    }

    pub(crate) fn set_current(&self, current: Option<u64>) {
        let previous = self.current.replace(current);
        if previous != current {
            for (index, (id, _)) in self.entries.iter().enumerate() {
                if Some(*id) == previous || Some(*id) == current {
                    self.notify.row_changed(index);
                }
            }
        }
    }
}

impl Model for QueueModel {
    type Data = QueueItem;

    fn row_count(&self) -> usize {
        self.entries.len()
    }

    fn row_data(&self, row: usize) -> Option<QueueItem> {
        self.entries.get(row).map(|(id, track)| QueueItem {
            id: id.to_string().into(),
            title: track.title.as_str().into(),
            artist: track
                .artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
                .into(),
            current: self.current.get() == Some(*id),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_queue_shares_metadata_and_marks_duplicate_occurrences_independently() {
        let track = Arc::new(Track {
            id: "fake:1".parse().unwrap(),
            title: "Same track".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            source_url: None,
            availability: eagle_core::Availability::Available,
            isrc: None,
        });
        let model = QueueModel::new(
            (1..=10_000).map(|id| (id, track.clone())).collect(),
            Some(1),
        );
        assert_eq!(model.row_count(), 10_000);
        assert_eq!(Arc::strong_count(&track), 10_001);
        assert_eq!(model.row_data(9999).unwrap().id, "10000");
        assert_eq!(model.row_data(9999).unwrap().title, "Same track");
        assert!(model.row_data(10_000).is_none());
        assert!(model.row_data(0).unwrap().current);
        assert!(!model.row_data(9999).unwrap().current);
        model.set_current(Some(10_000));
        assert!(!model.row_data(0).unwrap().current);
        assert!(model.row_data(9999).unwrap().current);
        model.set_current(None);
        assert!(!model.row_data(9999).unwrap().current);
        drop(model);
        assert_eq!(Arc::strong_count(&track), 1);
    }
}
