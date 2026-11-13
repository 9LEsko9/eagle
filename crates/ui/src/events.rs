// SPDX-License-Identifier: GPL-3.0-or-later
//! One scheduled UI callback, bounded pending artwork and ordered structural events.
use eagle_app::session::Event;
use std::collections::VecDeque;

pub(super) const IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub(super) struct Envelope {
    pub event: Event,
    pub epoch: u64,
    pub last_snapshot: bool,
}
#[derive(Default)]
pub(super) struct Batch {
    queue: VecDeque<Envelope>,
    pub scheduled: bool,
    bytes: usize,
    pub retry_artwork: bool,
}
fn image_bytes(event: &Event) -> usize {
    match event {
        Event::Thumbnail(_, cover, _) | Event::Cover(_, _, cover) => cover.rgba.len(),
        _ => 0,
    }
}
fn replaceable(event: &Event) -> bool {
    matches!(
        event,
        Event::Position(_) | Event::Player(_) | Event::Downloads(_) | Event::Thumbnail(_, _, _)
    )
}
fn same_slot(a: &Event, b: &Event) -> bool {
    match (a, b) {
        (Event::Position(_), Event::Position(_))
        | (Event::Player(_), Event::Player(_))
        | (Event::Downloads(_), Event::Downloads(_)) => true,
        (Event::Thumbnail(a, _, _), Event::Thumbnail(b, _, _)) => a == b,
        _ => false,
    }
}
impl Batch {
    pub fn push(&mut self, envelope: Envelope) {
        // A structural event is a barrier: replacements never move state across it.
        if replaceable(&envelope.event) {
            let candidate = self.queue.iter().rposition(|old| {
                !replaceable(&old.event)
                    || old.epoch != envelope.epoch
                    || same_slot(&old.event, &envelope.event)
            });
            if let Some(index) = candidate
                && self.queue[index].epoch == envelope.epoch
                && same_slot(&self.queue[index].event, &envelope.event)
                && let Some(old) = self.queue.remove(index)
            {
                self.bytes -= image_bytes(&old.event);
            }
        }
        let bytes = image_bytes(&envelope.event);
        while self.bytes.saturating_add(bytes) > IMAGE_BYTES {
            let Some(index) = self
                .queue
                .iter()
                .position(|old| image_bytes(&old.event) > 0)
            else {
                self.retry_artwork = true;
                return;
            };
            if let Some(old) = self.queue.remove(index) {
                self.bytes -= image_bytes(&old.event);
                self.retry_artwork = true;
            }
        }
        self.bytes += bytes;
        self.queue.push_back(envelope);
    }
    pub fn pop(&mut self) -> Option<Envelope> {
        let envelope = self.queue.pop_front()?;
        self.bytes -= image_bytes(&envelope.event);
        Some(envelope)
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn push(batch: &mut Batch, event: Event) {
        batch.push(Envelope {
            event,
            epoch: 0,
            last_snapshot: false,
        });
    }
    fn thumbnail(key: &str, bytes: usize) -> Event {
        Event::Thumbnail(
            key.into(),
            Arc::new(eagle_palette::Cover {
                rgba: vec![0; bytes],
                width: 1,
                height: 1,
                dominant: [0; 3],
            }),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    #[test]
    fn replacements_preserve_structural_order_and_generations() {
        let mut batch = Batch::default();
        push(&mut batch, Event::Downloads(vec![]));
        push(&mut batch, Event::Downloads(vec![]));
        push(&mut batch, Event::Busy(true));
        push(&mut batch, Event::Downloads(vec![]));
        batch.push(Envelope {
            event: Event::Downloads(vec![]),
            epoch: 1,
            last_snapshot: false,
        });
        assert_eq!(batch.queue.len(), 4);
        assert!(matches!(batch.pop().unwrap().event, Event::Downloads(_)));
        assert!(matches!(batch.pop().unwrap().event, Event::Busy(true)));
        assert_eq!(batch.pop().unwrap().epoch, 0);
        assert_eq!(batch.pop().unwrap().epoch, 1);
    }
    #[test]
    fn pending_artwork_is_bounded_and_latest_target_wins() {
        let mut batch = Batch::default();
        push(&mut batch, thumbnail("same", 4));
        push(&mut batch, thumbnail("same", IMAGE_BYTES));
        assert_eq!(batch.queue.len(), 1);
        push(&mut batch, Event::Error("keep action result".into()));
        push(&mut batch, thumbnail("next", IMAGE_BYTES));
        assert_eq!(batch.bytes, IMAGE_BYTES);
        assert!(batch.retry_artwork);
        assert!(matches!(batch.pop().unwrap().event, Event::Error(_)));
        assert!(matches!(batch.pop().unwrap().event, Event::Thumbnail(key, _, _) if key == "next"));
        push(&mut batch, thumbnail("oversized", IMAGE_BYTES + 1));
        assert!(batch.is_empty());
        assert_eq!(batch.bytes, 0);
    }
}
