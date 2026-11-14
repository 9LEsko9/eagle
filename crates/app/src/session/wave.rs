// SPDX-License-Identifier: GPL-3.0-or-later
//! «Моя волна»: the tuning the user sets, the feed shown beside it and the restarts that a change
//! of tuning causes. The provider decides how a tuning sounds; nothing here names a provider.
use super::*;
use eagle_core::RadioTuning;

/// A tuning that is still changing is sent to the provider only after it has rested this long.
pub(super) const TUNE_DEBOUNCE: Duration = Duration::from_millis(400);
/// Shown while a changed tuning has not reached the queue yet.
pub const MOVED_NOTE: &str = "Волна сместилась — изменится со следующего трека";
/// The provider is told about at most this many of the latest queued tracks.
pub(super) const RADIO_QUEUE_LIMIT: usize = 50;
const TUNING_KEY: &str = "radio_tuning";

/// What the user did with a track of the wave.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaveMark {
    #[default]
    None,
    /// «Не то».
    Skipped,
    Liked,
}

#[derive(Clone, Debug)]
pub struct WaveEntry {
    pub track: Arc<Track>,
    pub mark: WaveMark,
}

/// Everything the wave screen shows. Only tracks that came from the wave appear in it.
#[derive(Clone, Debug, Default)]
pub struct WaveState {
    /// Mood and novelty as stored; the seed is never part of it.
    pub tuning: RadioTuning,
    /// Up to two tracks before the current one, oldest first.
    pub previous: Vec<WaveEntry>,
    pub current: Option<WaveEntry>,
    pub next: Option<Arc<Track>>,
    /// Why the current track was chosen, from local data only; empty when nothing honest can be said.
    pub why: String,
    pub note: String,
}

/// A change of tuning or seed that waits for its turn.
#[derive(Clone)]
pub(super) struct Pending {
    at: Instant,
    seed: Option<TrackId>,
    /// Restart the wave; otherwise the tuning is only stored.
    restart: bool,
}

#[derive(Clone)]
pub(super) struct Wave {
    pub(super) tuning: RadioTuning,
    note: String,
    /// How many track changes the note survives; the «moved» note ends with the restart instead.
    note_keep: u8,
    marks: HashMap<u64, WaveMark>,
    pending: Option<Pending>,
    /// The batch that is being fetched starts playing when it arrives.
    pub(super) autoplay: bool,
    /// The batch that is being fetched replaces what follows the current track.
    pub(super) replace: bool,
    /// Occurrence the cached reason belongs to.
    why: Option<(u64, String)>,
}

impl Wave {
    pub(super) fn new(tuning: RadioTuning) -> Self {
        Self {
            tuning,
            note: String::new(),
            note_keep: 0,
            marks: HashMap::new(),
            pending: None,
            autoplay: false,
            replace: false,
            why: None,
        }
    }
    pub(super) fn reset(&mut self) {
        *self = Self::new(self.tuning.clone());
    }
    /// How long the worker may sleep before the pending change is due; `None` while nothing
    /// waits or a request is already in flight (its reply wakes the worker).
    pub(super) fn wait(&self, now: Instant, busy: bool) -> Option<Duration> {
        self.pending
            .as_ref()
            .filter(|_| !busy)
            .map(|pending| pending.at.saturating_duration_since(now))
    }
}

/// The tuning as the setting stores it: `{"mood":[x,y]|null,"novelty":n}`.
pub(super) fn encode_tuning(tuning: &RadioTuning) -> String {
    serde_json::json!({
        "mood": tuning.mood.map(|(x, y)| [x, y]),
        "novelty": tuning.novelty,
    })
    .to_string()
}

/// Reads a stored tuning; anything unreadable is the neutral wave.
pub(super) fn decode_tuning(json: &str) -> RadioTuning {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return RadioTuning::default();
    };
    let number = |value: &serde_json::Value| value.as_f64().map(|n| n as f32);
    let mood = value["mood"]
        .as_array()
        .filter(|pair| pair.len() == 2)
        .and_then(|pair| Some((number(&pair[0])?, number(&pair[1])?)));
    RadioTuning {
        mood,
        novelty: number(&value["novelty"]).unwrap_or(0.5),
        seed: None,
    }
    .normalized()
}

pub(super) fn load_tuning(store: &Store) -> RadioTuning {
    match store.setting(TUNING_KEY) {
        Ok(Some(json)) => decode_tuning(&json),
        Ok(None) => RadioTuning::default(),
        Err(error) => {
            log::warn!(error = error.to_string().as_str(); "wave tuning unreadable");
            RadioTuning::default()
        }
    }
}

/// An honest reason for the track, from what is stored on this device: a liked track by the
/// same artist, or the fact that the artist is new. `liked` and `history` come newest first.
pub fn wave_why(track: &Track, liked: &[Track], history: &[Track]) -> String {
    let Some(artist) = track.artists.first() else {
        return String::new();
    };
    let shares_artist = |other: &Track| {
        other.artists.iter().any(|candidate| {
            track.artists.iter().any(|own| {
                own.id == candidate.id || own.name.to_lowercase() == candidate.name.to_lowercase()
            })
        })
    };
    if let Some(similar) = liked
        .iter()
        .find(|other| other.id != track.id && shares_artist(other))
    {
        return format!("Похож на «{}» из твоих лайков", similar.title);
    }
    // The newest history entry is this very play once it has been recorded.
    let own_play = usize::from(history.first().is_some_and(|newest| newest.id == track.id));
    let heard = liked.iter().any(shares_artist) || history.iter().skip(own_play).any(shares_artist);
    if heard {
        String::new()
    } else {
        format!("Новое для тебя: {} раньше не звучал", artist.name)
    }
}

impl Worker {
    /// The wave track that is current, with its queue index.
    fn wave_current(&self) -> Option<(usize, Arc<Track>)> {
        self.wave_track(self.current)
            .map(|track| (self.current, track.clone()))
    }
    fn wave_track(&self, index: usize) -> Option<&Arc<Track>> {
        self.queue.get(index).filter(|track| {
            self.queue_radio
                .get(&self.occurrence_id(track))
                .is_some_and(|context| context.account == self.account)
        })
    }
    /// True while the current track came from the wave of the signed-in account.
    pub(super) fn wave_active(&self) -> bool {
        self.wave_current().is_some()
    }
    fn wave_mark(&self, track: &Arc<Track>) -> WaveMark {
        match self.wave.marks.get(&self.occurrence_id(track)) {
            Some(mark) => *mark,
            None if self.queue_likes.contains(&track.id) => WaveMark::Liked,
            None => WaveMark::None,
        }
    }
    pub(super) fn wave_state(&self) -> WaveState {
        let mut state = WaveState {
            tuning: self.wave.tuning.clone(),
            note: self.wave.note.clone(),
            ..WaveState::default()
        };
        let Some((index, current)) = self.wave_current() else {
            return state;
        };
        let entry = |track: &Arc<Track>| WaveEntry {
            track: track.clone(),
            mark: self.wave_mark(track),
        };
        state.previous = (index.saturating_sub(2)..index)
            .filter_map(|at| self.wave_track(at))
            .map(entry)
            .collect();
        state.current = Some(entry(&current));
        state.next = self.wave_track(index + 1).cloned();
        let occurrence = self.occurrence_id(&current);
        state.why = self
            .wave
            .why
            .as_ref()
            .filter(|(owner, _)| *owner == occurrence)
            .map(|(_, why)| why.clone())
            .unwrap_or_default();
        state
    }
    pub(super) fn emit_wave(&self) {
        (self.emit)(Event::Wave(Box::new(self.wave_state())));
    }
    /// The audible track changed: refresh the reason, retire old notes, keep the queue full.
    pub(super) fn wave_track_changed(&mut self) {
        if let Some((_, track)) = self.wave_current() {
            let occurrence = self.occurrence_id(&track);
            if self
                .wave
                .why
                .as_ref()
                .is_none_or(|(owner, _)| *owner != occurrence)
            {
                let why = self.compute_why(&track);
                self.wave.why = Some((occurrence, why));
            }
        }
        if self.wave.note_keep > 0 {
            self.wave.note_keep -= 1;
        } else if self.wave.note != MOVED_NOTE || (self.wave.pending.is_none() && !self.radio_busy)
        {
            self.wave.note.clear();
        }
        self.emit_wave();
        self.wave_refill();
    }
    fn compute_why(&self, track: &Track) -> String {
        let liked = self.store.liked_tracks(&[]).unwrap_or_else(|error| {
            log::warn!(error = error.to_string().as_str(); "wave reason: likes unreadable");
            Vec::new()
        });
        let history = self.store.history(&self.profile).unwrap_or_else(|error| {
            log::warn!(error = error.to_string().as_str(); "wave reason: history unreadable");
            Vec::new()
        });
        wave_why(track, &liked, &history)
    }
    /// Asks for more tracks before the queue of the wave runs dry, or starts a new session when
    /// the old one is gone (after a restart of the application).
    fn wave_refill(&mut self) {
        if self.radio_busy || self.wave.pending.is_some() || !self.wave_active() {
            return;
        }
        if self.queue.len().saturating_sub(self.current + 1) > 1 {
            return;
        }
        if self.radio_can_more && self.radio.is_some() {
            if let Err(error) = self.request_radio_more() {
                log::warn!(error = error.to_string().as_str(); "wave refill failed");
            }
        } else {
            self.wave.pending = Some(Pending {
                at: Instant::now(),
                seed: None,
                restart: true,
            });
        }
    }
    pub(super) fn wave_tune(&mut self, mood: Option<(f32, f32)>, novelty: f32) {
        let tuning = RadioTuning {
            mood,
            novelty,
            seed: None,
        }
        .normalized();
        if tuning == self.wave.tuning {
            return;
        }
        self.wave.tuning = tuning;
        let restart = self.wave_active();
        if restart {
            self.wave.note = MOVED_NOTE.into();
            self.wave.note_keep = 0;
        }
        let seed = self.wave.pending.as_ref().and_then(|p| p.seed.clone());
        let restart = restart || self.wave.pending.as_ref().is_some_and(|p| p.restart);
        self.wave.pending = Some(Pending {
            at: Instant::now() + TUNE_DEBOUNCE,
            seed,
            restart,
        });
        self.emit_wave();
    }
    pub(super) fn wave_more_like_this(&mut self) -> Result<()> {
        let (_, track) = self.wave_current().ok_or("волна сейчас не играет")?;
        self.wave.note = format!("Ещё такого — волна пойдёт от «{}»", track.title);
        self.wave.note_keep = 0;
        self.wave.pending = Some(Pending {
            at: Instant::now(),
            seed: Some(track.id.clone()),
            restart: true,
        });
        self.emit_wave();
        Ok(())
    }
    pub(super) fn wave_not_this(&mut self) -> Result<()> {
        let (_, track) = self.wave_current().ok_or("волна сейчас не играет")?;
        self.wave
            .marks
            .insert(self.occurrence_id(&track), WaveMark::Skipped);
        self.wave.note = format!("Понял — меньше похожего на «{}»", track.title);
        self.wave.note_keep = 1;
        if let Some(catalog) = &self.catalog {
            let _ = catalog.send(CatalogRequest::Dislike(track.id.clone()));
        }
        self.command(Command::Next)?;
        self.emit_wave();
        Ok(())
    }
    /// «Слушать волну»: start the wave and play it, or resume the one that is already there.
    pub(super) fn wave_play(&mut self) -> Result<()> {
        if self.view != View::Radio {
            return Err("откройте Мою волну".into());
        }
        if self.wave_active() {
            return self.command(Command::Resume);
        }
        self.start_radio(false, true, None)
    }
    /// Runs the pending change once it is due and no radio request is in flight.
    pub(super) fn wave_tick(&mut self, now: Instant) {
        let due = self
            .wave
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.at);
        if !due || self.radio_busy {
            return;
        }
        let Some(pending) = self.wave.pending.take() else {
            return;
        };
        if let Err(error) = self
            .store
            .put_setting(TUNING_KEY, encode_tuning(&self.wave.tuning))
        {
            log::warn!(error = error.to_string().as_str(); "wave tuning not saved");
        }
        if pending.restart
            && self.wave_active()
            && let Err(error) = self.start_radio(true, false, pending.seed)
        {
            (self.emit)(Event::Error(error.to_string()));
        }
    }
    /// Asks the catalog for a new session with the stored tuning. With `replace`, the answer
    /// takes the place of what follows the current track; with `autoplay` it starts playing.
    pub(super) fn start_radio(
        &mut self,
        replace: bool,
        autoplay: bool,
        seed: Option<TrackId>,
    ) -> Result<()> {
        if self.radio_busy {
            return Err("запрос радио уже выполняется".into());
        }
        if self.account.is_empty() {
            return Err("войдите в аккаунт для радио".into());
        }
        let tuning = RadioTuning {
            seed,
            ..self.wave.tuning.clone()
        };
        self.wave.replace = replace;
        self.wave.autoplay = autoplay;
        self.send_radio(CatalogRequest::RadioStart(
            self.view_generation,
            self.account.clone(),
            tuning,
        ));
        Ok(())
    }
    /// Asks the live session for its next batch.
    pub(super) fn request_radio_more(&mut self) -> Result<()> {
        if self.radio_busy {
            return Err("запрос радио уже выполняется".into());
        }
        if self.account.is_empty() {
            return Err("войдите в аккаунт для радио".into());
        }
        let (account, session, batches) = self
            .radio
            .as_ref()
            .filter(|(account, _, _)| account == &self.account && self.radio_can_more)
            .ok_or("начните новую радиосессию")?;
        let queue: Vec<_> = batches
            .iter()
            .flat_map(|batch| batch.tracks.iter())
            .map(|track| RadioQueueItem {
                track: track.id.clone(),
                album: track.album.clone(),
            })
            .collect();
        let skip = queue.len().saturating_sub(RADIO_QUEUE_LIMIT);
        let request = CatalogRequest::RadioMore(
            self.view_generation,
            account.clone(),
            session.clone(),
            queue.into_iter().skip(skip).collect(),
        );
        self.send_radio(request);
        Ok(())
    }
    fn send_radio(&mut self, request: CatalogRequest) {
        if self
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog.send(request).is_ok())
        {
            self.radio_busy = true;
            self.radio_notice = None;
        } else {
            self.wave.replace = false;
            self.wave.autoplay = false;
            self.radio_notice = Some("каталог остановлен".into());
        }
        self.emit_radio_state();
    }
    /// Swaps what follows the current track for the new session's first batch. The current
    /// track keeps playing; the track the new session may start with is dropped.
    pub(super) fn replace_upcoming(
        &mut self,
        account: &str,
        session_id: &str,
        batch: &RadioBatch,
    ) -> Result<()> {
        let keep = (self.current + 1).min(self.queue.len());
        let removed = self.queue.split_off(keep);
        let gone: HashSet<usize> = removed
            .iter()
            .map(|track| Arc::as_ptr(track) as usize)
            .collect();
        for pointer in &gone {
            if let Some(id) = self.queue_ids.get(pointer) {
                self.queue_radio.remove(id);
                self.wave.marks.remove(id);
            }
        }
        self.queue_original
            .retain(|track| !gone.contains(&(Arc::as_ptr(track) as usize)));
        let playing = self.queue.get(self.current).map(|track| track.id.clone());
        let batch = RadioBatch {
            id: batch.id.clone(),
            tracks: batch
                .tracks
                .iter()
                .filter(|track| Some(&track.id) != playing.as_ref())
                .cloned()
                .collect(),
        };
        self.push_radio_batch(account, session_id, &batch)
    }
    /// Plays the first available track of the batch that was just published as the list.
    pub(super) fn wave_autoplay(&mut self) {
        let Some(index) = self.rows.iter().position(|row| {
            row.blocked_reason.is_none() && row.track.availability == Availability::Available
        }) else {
            self.radio_notice = Some("В подборке нет доступных треков".into());
            return;
        };
        if let Err(error) = self.play_row(index).and_then(|()| self.checkpoint()) {
            self.radio_notice = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_core::{Artist, Availability};

    fn track(id: &str, title: &str, artist: &str) -> Track {
        Track {
            id: format!("yandex:{id}").parse().unwrap(),
            title: title.into(),
            artists: vec![Artist {
                id: format!(
                    "yandex:a{}",
                    artist
                        .bytes()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                )
                .parse()
                .unwrap(),
                name: artist.into(),
                cover: None,
            }],
            album: None,
            album_title: None,
            duration_ms: 1000,
            cover: None,
            availability: Availability::Available,
            isrc: None,
        }
    }

    #[test]
    fn why_names_a_liked_track_of_the_same_artist() {
        let current = track("1", "Новая", "Сигнал");
        let liked = vec![track("9", "Другая", "Мира"), track("2", "Старая", "Сигнал")];
        assert_eq!(
            wave_why(&current, &liked, &[]),
            "Похож на «Старая» из твоих лайков"
        );
    }

    #[test]
    fn why_ignores_the_track_itself_and_matches_names_across_providers() {
        let current = track("1", "Новая", "Сигнал");
        let mut elsewhere = track("5", "Из другого", "сигнал");
        elsewhere.id = "other:5".parse().unwrap();
        elsewhere.artists[0].id = "other:a".parse().unwrap();
        let only_itself = vec![current.clone()];
        assert_eq!(wave_why(&current, &only_itself, &[]), "");
        assert_eq!(
            wave_why(&current, &[elsewhere], &[]),
            "Похож на «Из другого» из твоих лайков"
        );
    }

    #[test]
    fn why_calls_an_unheard_artist_new_but_not_one_in_history() {
        let current = track("1", "Новая", "Сигнал");
        assert_eq!(
            wave_why(&current, &[], &[]),
            "Новое для тебя: Сигнал раньше не звучал"
        );
        // Only the play that is under way: still new.
        assert_eq!(
            wave_why(&current, &[], std::slice::from_ref(&current)),
            "Новое для тебя: Сигнал раньше не звучал"
        );
        let heard = [track("1", "Новая", "Сигнал"), track("3", "Давно", "Сигнал")];
        assert_eq!(wave_why(&current, &[], &heard), "");
        // An earlier play of the very same track counts as heard.
        let replay = [track("1", "Новая", "Сигнал"), track("1", "Новая", "Сигнал")];
        assert_eq!(wave_why(&current, &[], &replay), "");
    }

    #[test]
    fn why_is_empty_without_an_artist() {
        let mut current = track("1", "Без исполнителя", "x");
        current.artists.clear();
        assert_eq!(wave_why(&current, &[], &[]), "");
    }

    #[test]
    fn tuning_survives_the_round_trip_and_bad_input_is_neutral() {
        let tuning = RadioTuning {
            mood: Some((0.5, -0.25)),
            novelty: 0.75,
            seed: None,
        };
        assert_eq!(decode_tuning(&encode_tuning(&tuning)), tuning);
        let any = RadioTuning {
            mood: None,
            novelty: 0.1,
            seed: None,
        };
        assert_eq!(decode_tuning(&encode_tuning(&any)), any);
        for broken in ["", "nope", "[]", r#"{"mood":[1],"novelty":"x"}"#] {
            assert_eq!(decode_tuning(broken), RadioTuning::default(), "{broken}");
        }
    }
}
