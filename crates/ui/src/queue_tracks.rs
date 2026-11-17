// SPDX-License-Identifier: GPL-3.0-or-later
//! Upcoming tracks: shared metadata, with artwork retained only around the viewport.
use crate::{ProviderItem, TrackItem};
use eagle_app::session::LibraryItem;
use eagle_core::{Track, TrackId};
use slint::{Image, Model, ModelNotify, ModelTracker};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    sync::Arc,
};

#[derive(Default)]
pub(crate) struct QueueTracks {
    entries: RefCell<Vec<(Option<u64>, Arc<Track>)>>,
    providers: RefCell<slint::ModelRc<ProviderItem>>,
    window: Cell<(usize, usize)>,
    enabled: Cell<bool>,
    images: RefCell<HashMap<usize, Image>>,
    notify: ModelNotify,
}

impl QueueTracks {
    pub(crate) fn reset(&self, entries: Vec<(Option<u64>, Arc<Track>)>) {
        let old = self.entries.borrow();
        let known: HashMap<usize, Image> = self
            .images
            .borrow()
            .iter()
            .filter_map(|(row, image)| {
                old.get(*row)
                    .map(|(_, track)| (Arc::as_ptr(track) as usize, image.clone()))
            })
            .collect();
        drop(old);
        *self.images.borrow_mut() = entries
            .iter()
            .enumerate()
            .filter_map(|(row, (_, track))| {
                known
                    .get(&(Arc::as_ptr(track) as usize))
                    .map(|image| (row, image.clone()))
            })
            .collect();
        *self.entries.borrow_mut() = entries;
        self.notify.reset();
    }

    pub(crate) fn set_providers(&self, providers: slint::ModelRc<ProviderItem>) {
        *self.providers.borrow_mut() = providers;
    }

    fn range(&self) -> Range<usize> {
        let (first, count) = self.window.get();
        let len = self.row_count();
        if !self.enabled.get() || count == 0 {
            return 0..0;
        }
        first.saturating_sub(8).min(len)..first.saturating_add(count).saturating_add(8).min(len)
    }

    fn active_rows(&self) -> impl Iterator<Item = usize> {
        // The cover strip also displays the next track when the list is scrolled.
        (self.enabled.get() && self.row_count() > 0)
            .then_some(0)
            .into_iter()
            .chain(self.range().filter(|index| *index != 0))
    }

    pub(crate) fn visible(&self, first: usize, count: usize, enabled: bool) -> Vec<String> {
        self.window.set((first, count));
        self.refresh(enabled)
    }

    pub(crate) fn refresh(&self, enabled: bool) -> Vec<String> {
        self.enabled.set(enabled);
        let range = self.range();
        let mut evicted = Vec::new();
        self.images.borrow_mut().retain(|index, _| {
            let keep = enabled && (*index == 0 || range.contains(index));
            if !keep {
                evicted.push(*index);
            }
            keep
        });
        for index in evicted {
            self.notify.row_changed(index);
        }
        let entries = self.entries.borrow();
        let images = self.images.borrow();
        self.active_rows()
            .filter(|index| !images.contains_key(index))
            .map(|index| format!("t:{}", entries[index].1.id))
            .collect()
    }

    pub(crate) fn matching_rows(&self, id: &str) -> Vec<usize> {
        let Ok(id) = id.parse::<TrackId>() else {
            return Vec::new();
        };
        let entries = self.entries.borrow();
        self.active_rows()
            .filter(|index| entries[*index].1.id == id)
            .collect()
    }

    pub(crate) fn set_cover(&self, index: usize, image: Image) {
        if self.active_rows().any(|row| row == index) {
            self.images.borrow_mut().insert(index, image);
            self.notify.row_changed(index);
        }
    }

    pub(crate) fn cached_images(&self) -> Vec<Image> {
        self.images.borrow().values().cloned().collect()
    }
}

impl Model for QueueTracks {
    type Data = TrackItem;

    fn row_count(&self) -> usize {
        self.entries.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<TrackItem> {
        let entries = self.entries.borrow();
        let (occurrence, track) = entries.get(row)?;
        let mut item = crate::bridge::track_item(
            LibraryItem {
                sources: Vec::new(),
                blocked_reason: None,
                track: track.clone(),
                liked: false,
            },
            &self.providers.borrow(),
        );
        item.occurrence = occurrence
            .map_or_else(String::new, |id| id.to_string())
            .into();
        item.cover = self.images.borrow().get(&row).cloned().unwrap_or_default();
        Some(item)
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
    fn large_queue_keeps_occurrences_but_bounds_artwork_after_scroll_close_and_reset() {
        let track = Arc::new(Track {
            id: "fake:1".parse().unwrap(),
            title: "Repeated track".into(),
            artists: vec![],
            album: None,
            album_title: None,
            duration_ms: 123_000,
            cover: None,
            source_url: None,
            availability: eagle_core::Availability::Available,
            isrc: None,
        });
        let model = QueueTracks::default();
        model.reset((1..=10_000).map(|id| (Some(id), track.clone())).collect());
        assert_eq!(model.row_count(), 10_000);
        assert_eq!(Arc::strong_count(&track), 10_001);
        let row = model.row_data(9999).unwrap();
        assert_eq!(row.occurrence, "10000");
        assert_eq!(row.title, "Repeated track");
        assert_eq!(row.duration, "2:03");
        assert!(model.row_data(10_000).is_none());
        assert!(model.matching_rows("fake:1").is_empty());
        assert_eq!(model.visible(1000, 10, true).len(), 27);
        let image = Image::from_rgba8(slint::SharedPixelBuffer::new(1, 1));
        for index in model.matching_rows("fake:1") {
            model.set_cover(index, image.clone());
        }
        assert_eq!(model.cached_images().len(), 27);
        assert!(model.refresh(true).is_empty());
        assert_eq!(model.visible(9990, 10, true).len(), 18);
        assert_eq!(model.cached_images().len(), 1);
        model.set_cover(1000, image.clone());
        assert_eq!(model.row_data(1000).unwrap().cover.size().width, 0);
        assert_eq!(model.row_data(0).unwrap().cover, image);
        assert!(model.refresh(false).is_empty());
        assert!(model.cached_images().is_empty());
        assert!(model.matching_rows("fake:1").is_empty());
        model.reset(vec![]);
        assert_eq!(model.row_count(), 0);
        assert_eq!(Arc::strong_count(&track), 1);
    }
}
