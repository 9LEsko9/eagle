// SPDX-License-Identifier: GPL-3.0-or-later
//! Choosing which provider's copy of a track to play.
use crate::providers::Providers;
use eagle_core::{Encryption, ProviderError, Quality, Stream, TrackId};
use eagle_net::Client;
use eagle_store::{Cache, Store};
use std::sync::{Arc, Mutex};

/// Copies of `id` across providers, in the order to try them: ones already stored locally,
/// then `id` itself, then the rest of its group by provider priority.
pub(crate) fn candidates(
    store: &Store,
    providers: &Providers,
    id: &TrackId,
    is_stored: impl Fn(&TrackId) -> bool,
) -> Vec<TrackId> {
    let mut members = match store.group_of(id) {
        Ok(Some(grp)) => store.group_members(grp).unwrap_or_default(),
        _ => Vec::new(),
    };
    if !members.contains(id) {
        members.push(id.clone());
    }
    let priority = |track: &TrackId| {
        providers
            .all()
            .iter()
            .position(|p| p.info().id == track.provider())
            .unwrap_or(usize::MAX)
    };
    members.retain(|track| priority(track) != usize::MAX);
    members.sort_by_key(|track| (!is_stored(track), track != id, priority(track)));
    members
}

/// Whether a failure of one candidate justifies trying the next one.
/// Transient errors move on only to a different provider: the same provider would fail again.
fn tries_next(error: &ProviderError, same_provider_next: bool) -> bool {
    match error {
        ProviderError::Network | ProviderError::RateLimited => !same_provider_next,
        ProviderError::Unavailable
        | ProviderError::UnsupportedQuality
        | ProviderError::NotFound
        | ProviderError::Auth
        | ProviderError::Uncertain
        | ProviderError::ApiChanged => true,
        _ => false,
    }
}

/// Resolves the first candidate that can be played and returns it with its stream.
/// Signed-out providers are skipped; if nothing was tried the result is `Auth`.
pub(crate) fn resolve_with_fallback(
    providers: &Providers,
    candidates: &[TrackId],
    quality: Quality,
) -> Result<(TrackId, Stream), ProviderError> {
    let mut last = ProviderError::Auth;
    for (at, id) in candidates.iter().enumerate() {
        let Some(provider) = providers.for_track(id) else {
            continue;
        };
        if !providers.is_signed_in(provider.as_ref()) {
            continue;
        }
        match provider.resolve_stream(id, quality) {
            Ok(stream) => return Ok((id.clone(), stream)),
            Err(error) => {
                let same_provider_next = candidates
                    .get(at + 1)
                    .is_some_and(|next| next.provider() == id.provider());
                log::warn!(provider = id.provider(), track_id = id.to_string().as_str(); "источник недоступен: {error}");
                let next = tries_next(&error, same_provider_next);
                last = error;
                if !next {
                    break;
                }
            }
        }
    }
    Err(last)
}

/// Everything needed to pick and open the copy of a track that will actually play.
pub(crate) struct SourceContext {
    pub providers: Arc<Providers>,
    pub store: Arc<Store>,
    pub cache: Arc<Cache>,
    pub net: Arc<Client>,
    /// Provider whose account owns offline rights, and that account's uid.
    pub account_provider: Option<String>,
    pub account: String,
    pub requested: TrackId,
    pub on_bitrate: Arc<dyn Fn(Option<u32>) + Send + Sync>,
    /// Called with the track that was really chosen, before it starts playing.
    pub on_source: Arc<dyn Fn(&TrackId) + Send + Sync>,
}

/// A source that picks its copy on first open (on the decoder thread) and keeps it afterwards,
/// so reopening after a seek reads the same file and cache entry.
pub(crate) fn lazy_source(ctx: SourceContext) -> eagle_engine::Source {
    let chosen: Mutex<Option<eagle_engine::Source>> = Mutex::new(None);
    eagle_engine::Source::new(move |interrupt| {
        let source = {
            let mut slot = chosen.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(ctx.choose()?);
            }
            slot.clone()
        }
        .ok_or("источник не выбран")?;
        source.open_reader(interrupt)
    })
}

impl SourceContext {
    fn choose(&self) -> Result<eagle_engine::Source, eagle_engine::SourceError> {
        let stored = |id: &TrackId| {
            [Quality::High, Quality::Standard]
                .into_iter()
                .any(|q| matches!(self.cache.lookup(&self.store, id, q), Ok(Some(_))))
        };
        let list = candidates(&self.store, &self.providers, &self.requested, stored);
        let first = list.first().cloned().ok_or("провайдер недоступен")?;
        let prefetched = if stored(&first) {
            None
        } else {
            Some(resolve_with_fallback(
                &self.providers,
                &list,
                Quality::High,
            )?)
        };
        let id = prefetched.as_ref().map_or(first, |(id, _)| id.clone());
        (self.on_source)(&id);
        let owner = if self.account_provider.as_deref() == Some(id.provider()) {
            self.account.clone()
        } else {
            String::new()
        };
        let provider = self
            .providers
            .for_track(&id)
            .ok_or("провайдер недоступен")?
            .clone();
        let first_stream = Mutex::new(prefetched.map(|(_, stream)| stream));
        let owner_store = self.store.clone();
        let owner_id = id.clone();
        let owner_uid = owner.clone();
        let resolve: Arc<eagle_net::growing::Resolve> = Arc::new(move || {
            if !owner_uid.is_empty()
                && provider
                    .account()
                    .map_err(|_| eagle_net::Error::Source("не удалось подтвердить аккаунт"))?
                    .id
                    != owner_uid
            {
                return Err(eagle_net::Error::Source(
                    "аккаунт воспроизведения изменился",
                ));
            }
            let ready = first_stream
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            let stream = match ready {
                Some(stream) => stream,
                None => provider
                    .resolve_stream(&owner_id, Quality::High)
                    .map_err(|_| eagle_net::Error::Source("не удалось получить аудиоссылку"))?,
            };
            if !matches!(stream.encryption, Encryption::None) {
                return Err(eagle_net::Error::Source(
                    "шифрованный поток не поддерживается",
                ));
            }
            if !owner_uid.is_empty() {
                owner_store
                    .record_audio_owner(&owner_uid, &owner_id.to_string())
                    .map_err(|_| {
                        eagle_net::Error::Source("не удалось сохранить права на локальный файл")
                    })?;
            }
            Ok(eagle_net::growing::Location {
                bitrate_kbps: stream.bitrate_kbps,
                url: stream.url,
                expires_at: Some(stream.expires_at),
            })
        });
        let authorize_resolve = resolve.clone();
        let authorize_store = self.store.clone();
        let authorize_owner = owner.clone();
        let authorize_id = id.to_string();
        let source = crate::playback::streamed_with_hooks(
            self.store.clone(),
            self.cache.clone(),
            self.net.clone(),
            id.clone(),
            Quality::High,
            resolve,
            crate::playback::PlaybackHooks {
                on_bitrate: self.on_bitrate.clone(),
                authorize_local: Arc::new(move || {
                    // Providers without an account have no offline rights to check.
                    if authorize_owner.is_empty()
                        || authorize_store.allows_local_playback(&authorize_owner, &authorize_id)?
                    {
                        return Ok(true);
                    }
                    authorize_resolve()?;
                    Ok(authorize_store.owns_audio(&authorize_owner, &authorize_id)?)
                }),
            },
        )
        .map_err(|e| e.to_string())?;
        let store = self.store.clone();
        let check_id = id.to_string();
        Ok(eagle_engine::Source::new(move |interrupt| {
            if !owner.is_empty() && !store.access(&owner, &check_id)?.allows_playback() {
                return Err("Провайдер подтвердил потерю доступа к сохранённому треку".into());
            }
            source.open_reader(interrupt)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_core::{Account, Provider};
    use eagle_provider_sdk::testkit::{FakeProvider, fake_track};
    use std::sync::Arc;

    fn signed_in(providers: &Providers) {
        for p in providers.all() {
            let account = Account {
                id: "1".into(),
                name: None,
                subscription: true,
                capabilities: p.info().capabilities,
            };
            providers.set_account(p.info().id, Some(account));
        }
    }
    fn setup(
        a: FakeProvider,
        b: FakeProvider,
    ) -> (Providers, Arc<FakeProvider>, Arc<FakeProvider>) {
        let (a, b) = (Arc::new(a), Arc::new(b));
        let all: Vec<Arc<dyn Provider>> = vec![a.clone(), b.clone()];
        let providers = Providers::new(all, &[]);
        signed_in(&providers);
        (providers, a, b)
    }
    fn catalog(provider: &str) -> Vec<eagle_core::Track> {
        vec![fake_track(provider, "1", "Song", "Artist", 180_000)]
    }
    fn id(text: &str) -> TrackId {
        text.parse().unwrap()
    }

    #[test]
    fn unavailable_track_plays_the_other_providers_copy() {
        let (providers, a, b) = setup(
            FakeProvider::new("fakea")
                .with_tracks(catalog("fakea"))
                .with_unavailable(&["1"]),
            FakeProvider::new("fakeb").with_tracks(catalog("fakeb")),
        );
        let list = [id("fakea:1"), id("fakeb:1")];
        let (played, _) = resolve_with_fallback(&providers, &list, Quality::High).unwrap();
        assert_eq!(played, id("fakeb:1"));
        assert_eq!(a.resolve_calls().len(), 1);
        assert_eq!(b.resolve_calls().len(), 1);
    }

    #[test]
    fn network_error_of_the_only_candidate_does_not_switch() {
        let (providers, _, b) = setup(
            FakeProvider::new("fakea")
                .with_tracks(catalog("fakea"))
                .fail_with(|| ProviderError::Network),
            FakeProvider::new("fakeb").with_tracks(catalog("fakeb")),
        );
        let result = resolve_with_fallback(&providers, &[id("fakea:1")], Quality::High);
        assert!(matches!(result, Err(ProviderError::Network)));
        assert!(b.resolve_calls().is_empty());
    }

    #[test]
    fn network_error_moves_on_to_another_provider() {
        let (providers, _, _) = setup(
            FakeProvider::new("fakea")
                .with_tracks(catalog("fakea"))
                .fail_with(|| ProviderError::Network),
            FakeProvider::new("fakeb").with_tracks(catalog("fakeb")),
        );
        let list = [id("fakea:1"), id("fakeb:1")];
        let (played, _) = resolve_with_fallback(&providers, &list, Quality::High).unwrap();
        assert_eq!(played, id("fakeb:1"));
    }

    #[test]
    fn signed_out_provider_is_skipped_without_a_call() {
        let (providers, a, _) = setup(
            FakeProvider::new("fakea").with_tracks(catalog("fakea")),
            FakeProvider::new("fakeb").with_tracks(catalog("fakeb")),
        );
        providers.set_account("fakea", None);
        let list = [id("fakea:1"), id("fakeb:1")];
        let (played, _) = resolve_with_fallback(&providers, &list, Quality::High).unwrap();
        assert_eq!(played, id("fakeb:1"));
        assert!(a.resolve_calls().is_empty());
    }

    #[test]
    fn nothing_signed_in_reports_auth() {
        let (providers, _, _) = setup(
            FakeProvider::new("fakea").with_tracks(catalog("fakea")),
            FakeProvider::new("fakeb"),
        );
        providers.set_account("fakea", None);
        let result = resolve_with_fallback(&providers, &[id("fakea:1")], Quality::High);
        assert!(matches!(result, Err(ProviderError::Auth)));
    }

    #[test]
    fn candidates_put_stored_first_then_requested_then_priority() {
        let dir = std::env::temp_dir().join(format!("eg-source-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let (providers, _, _) = setup(FakeProvider::new("fakea"), FakeProvider::new("fakeb"));
        let a = fake_track("fakea", "1", "Song", "Artist", 180_000);
        let b = fake_track("fakeb", "9", "Song", "Artist", 180_500);
        store.put_tracks(&[a, b]).unwrap();
        let (a, b) = (id("fakea:1"), id("fakeb:9"));
        assert_eq!(
            candidates(&store, &providers, &b, |_| false),
            [b.clone(), a.clone()]
        );
        assert_eq!(
            candidates(&store, &providers, &a, |_| false),
            [a.clone(), b.clone()]
        );
        assert_eq!(candidates(&store, &providers, &a, |t| *t == b), [b, a]);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
