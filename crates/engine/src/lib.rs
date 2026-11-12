// SPDX-License-Identifier: GPL-3.0-or-later
//! The engine only receives local audio, never provider-specific IDs or URLs.
use eagle_core::{Availability, Track};
use std::{collections::VecDeque, fs::File, io::BufReader, path::Path};
#[derive(Default)]
pub struct Queue {
    tracks: VecDeque<Track>,
}
impl Queue {
    pub fn push(&mut self, track: Track) {
        self.tracks.push_back(track);
    }
    pub fn next_available(&mut self) -> Option<Track> {
        while let Some(t) = self.tracks.pop_front() {
            if t.availability == Availability::Available {
                return Some(t);
            }
        }
        None
    }
}
pub fn play(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (_stream, handle) = rodio::OutputStream::try_default()?;
    let sink = rodio::Sink::try_new(&handle)?;
    sink.append(rodio::Decoder::new(BufReader::new(File::open(path)?))?);
    sink.sleep_until_end();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_queue_skips_unavailable_tracks() {
        let track = |id: &str, availability| Track {
            id: id.parse().unwrap(),
            title: String::new(),
            artists: vec![],
            album: None,
            duration_ms: 1,
            cover: None,
            availability,
        };
        let mut q = Queue::default();
        q.push(track("yandex:1", Availability::Region));
        q.push(track("vk:2", Availability::Available));
        q.push(track("yandex:3", Availability::Available));
        assert_eq!(q.next_available().unwrap().id.to_string(), "vk:2");
        assert_eq!(q.next_available().unwrap().id.to_string(), "yandex:3");
        assert!(q.next_available().is_none());
    }
}
