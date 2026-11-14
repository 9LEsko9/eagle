// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Conformance checks a provider runs from its own tests against recorded or fake responses.

use crate::{
    Page, Provider, ProviderError, ProviderInfo, Quality, RadioError, RadioTuning, Track, TrackId,
};
use std::{fmt, time::SystemTime};

/// Upper bound for followed `next` cursors; longer chains are reported as endless.
const MAX_PAGES: usize = 100;

/// Data the checks need from the provider's test account.
pub struct Fixtures {
    /// A track that `resolve_stream` must be able to resolve.
    pub known_track: TrackId,
    /// Query whose search results must contain tracks with `isrc` when the flag is claimed.
    pub query: String,
    /// Artist for the artist screens; those checks are skipped when `None`.
    pub artist: Option<TrackId>,
    /// Whether the test account is signed in.
    pub signed_in: bool,
}

/// One failed check; `check` is a stable name, `detail` is for humans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub check: &'static str,
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.check, self.detail)
    }
}

/// Checks the self-description: a `[a-z]+` id and a non-empty name.
pub fn check_info(info: &ProviderInfo) -> Vec<Violation> {
    let mut out = Vec::new();
    if info.id.is_empty() || !info.id.bytes().all(|b| b.is_ascii_lowercase()) {
        out.push(Violation {
            check: "info.id",
            detail: format!("id {:?} должен состоять из [a-z]+", info.id),
        });
    }
    if info.name.trim().is_empty() {
        out.push(Violation {
            check: "info.name",
            detail: "name не должен быть пустым".into(),
        });
    }
    out
}

/// Runs every conformance check and returns all violations (empty means conformant).
pub fn run(provider: &dyn Provider, fixtures: &Fixtures) -> Vec<Violation> {
    let info = provider.info();
    let mut out = check_info(info);
    check_track_ids(provider, fixtures, &mut out);
    check_capabilities(provider, fixtures, &mut out);
    check_pagination(provider, &mut out);
    check_stream(provider, fixtures, &mut out);
    if info.capabilities.isrc {
        check_isrc(provider, fixtures, &mut out);
    }
    out
}

/// True when the result is an `Auth` error for an account that is not signed in.
fn expected_auth(err: &ProviderError, fixtures: &Fixtures) -> bool {
    !fixtures.signed_in && matches!(err, ProviderError::Auth)
}

fn radio_error(err: RadioError) -> ProviderError {
    match err {
        RadioError::Provider(err) => err,
        _ => ProviderError::Uncertain,
    }
}

/// Reports a claimed capability whose method answered `Unavailable`.
fn report_capability(
    check: &'static str,
    result: Result<(), ProviderError>,
    fixtures: &Fixtures,
    out: &mut Vec<Violation>,
) {
    match result {
        Ok(()) => {}
        Err(err) if expected_auth(&err, fixtures) => {}
        Err(ProviderError::Unavailable) => out.push(Violation {
            check,
            detail: "возможность заявлена, но метод возвращает Unavailable".into(),
        }),
        Err(_) => {}
    }
}

fn check_track_ids(provider: &dyn Provider, fixtures: &Fixtures, out: &mut Vec<Violation>) {
    let info_id = provider.info().id;
    let mut tracks: Vec<Track> = Vec::new();
    if let Ok(page) = provider.search(&fixtures.query, 0) {
        tracks.extend(page.items);
    }
    if fixtures.signed_in
        && let Ok(page) = provider.likes(None)
    {
        tracks.extend(page.items);
    }
    for track in tracks {
        let text = track.id.to_string();
        let valid = text
            .parse::<TrackId>()
            .is_ok_and(|parsed| parsed == track.id && track.id.provider() == info_id);
        if !valid {
            out.push(Violation {
                check: "track.id",
                detail: format!(
                    "трек {text} не проходит TrackId::from_str или принадлежит не провайдеру {info_id}"
                ),
            });
        }
    }
}

fn check_capabilities(provider: &dyn Provider, fixtures: &Fixtures, out: &mut Vec<Violation>) {
    let caps = provider.info().capabilities;
    let query = fixtures.query.as_str();
    if caps.search_tracks {
        let result = provider.search(query, 0).map(drop);
        report_capability("capability.search_tracks", result, fixtures, out);
    }
    if caps.search_artists {
        let result = provider.search_artists(query, 0).map(drop);
        report_capability("capability.search_artists", result, fixtures, out);
    }
    if caps.search_albums {
        let result = provider.search_albums(query, 0).map(drop);
        report_capability("capability.search_albums", result, fixtures, out);
    }
    if caps.search_playlists {
        let result = provider.search_playlists(query, 0).map(drop);
        report_capability("capability.search_playlists", result, fixtures, out);
    }
    if caps.likes {
        let result = provider.likes(None).map(drop);
        report_capability("capability.likes", result, fixtures, out);
    }
    if caps.playlists_read {
        let result = provider.playlists(None).map(drop);
        report_capability("capability.playlists_read", result, fixtures, out);
    }
    if let Some(artist) = &fixtures.artist {
        if caps.artist_page {
            let result = provider.artist(artist).map(drop);
            report_capability("capability.artist_page", result, fixtures, out);
        }
        if caps.artist_all_tracks {
            let result = provider.artist_tracks(artist, None).map(drop);
            report_capability("capability.artist_all_tracks", result, fixtures, out);
        }
    }
    if caps.radio {
        let result = provider
            .radio_start(&RadioTuning::default())
            .map(drop)
            .map_err(radio_error);
        report_capability("capability.radio", result, fixtures, out);
        if caps.radio_tuning {
            let tuning = RadioTuning {
                mood: Some((0.0, 1.0)),
                novelty: 0.9,
                seed: None,
            };
            let result = provider.radio_start(&tuning).map(drop).map_err(radio_error);
            report_capability("capability.radio_tuning", result, fixtures, out);
        }
    }
}

fn check_pagination(provider: &dyn Provider, out: &mut Vec<Violation>) {
    let caps = provider.info().capabilities;
    if caps.search_tracks && !search_pages_end(provider) {
        out.push(Violation {
            check: "pagination",
            detail: format!("search: больше {MAX_PAGES} страниц или next не завершается"),
        });
    }
    if caps.likes && !likes_pages_end(provider) {
        out.push(Violation {
            check: "pagination",
            detail: format!("likes: больше {MAX_PAGES} страниц или next не завершается"),
        });
    }
}

/// Search pages are numbered: the `next` value is the number of the following page.
fn search_pages_end(provider: &dyn Provider) -> bool {
    let mut page = 0u32;
    for _ in 0..MAX_PAGES {
        match provider.search("", page) {
            Ok(Page {
                next: Some(next), ..
            }) => match next.parse::<u32>() {
                Ok(number) => page = number,
                Err(_) => return true,
            },
            Ok(_) | Err(_) => return true,
        }
    }
    false
}

/// Cursors are opaque; only the chain's length is checked.
fn likes_pages_end(provider: &dyn Provider) -> bool {
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        match provider.likes(cursor.as_deref()) {
            Ok(Page {
                next: Some(next), ..
            }) => cursor = Some(next),
            Ok(_) | Err(_) => return true,
        }
    }
    false
}

fn check_stream(provider: &dyn Provider, fixtures: &Fixtures, out: &mut Vec<Violation>) {
    match provider.resolve_stream(&fixtures.known_track, Quality::Standard) {
        Ok(stream) => {
            if stream.url.expose().is_empty() {
                out.push(Violation {
                    check: "stream.url",
                    detail: "resolve_stream вернул пустой URL".into(),
                });
            }
            if stream.expires_at <= SystemTime::now() {
                out.push(Violation {
                    check: "stream.expires_at",
                    detail: "срок жизни ссылки уже истёк".into(),
                });
            }
        }
        Err(err) if expected_auth(&err, fixtures) => {}
        Err(err) => out.push(Violation {
            check: "stream.resolve",
            detail: format!(
                "resolve_stream({}) вернул ошибку: {err}",
                fixtures.known_track
            ),
        }),
    }
}

fn check_isrc(provider: &dyn Provider, fixtures: &Fixtures, out: &mut Vec<Violation>) {
    match provider.search(&fixtures.query, 0) {
        Ok(page) if page.items.iter().any(|t| t.isrc.is_some()) => {}
        Ok(_) => out.push(Violation {
            check: "isrc",
            detail: "isrc заявлен, но ни один трек из поиска не содержит ISRC".into(),
        }),
        Err(err) if expected_auth(&err, fixtures) => {}
        Err(err) => out.push(Violation {
            check: "isrc",
            detail: format!("поиск для проверки ISRC вернул ошибку: {err}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{FakeProvider, fake_track};
    use crate::{
        Account, Album, Artist, ArtistPage, Capabilities, Playlist, PlaylistEdit,
        PlaylistEditError, PlaylistSnapshot, RadioSession, Stream, Token,
    };
    use std::time::Duration;

    fn catalog(id: &'static str) -> Vec<Track> {
        vec![
            fake_track(id, "t1", "Song One", "Artist", 180_000),
            fake_track(id, "t2", "Song Two", "Artist", 200_000),
        ]
    }

    fn fixtures(id: &str, signed_in: bool) -> Fixtures {
        Fixtures {
            known_track: format!("{id}:t1").parse().unwrap(),
            query: "song".into(),
            artist: Some(format!("{id}:artist-artist").parse().unwrap()),
            signed_in,
        }
    }

    fn has(violations: &[Violation], check: &str) -> bool {
        violations.iter().any(|v| v.check == check)
    }

    /// Delegates to a `FakeProvider` and breaks exactly the behaviour a test names.
    struct Mutant {
        inner: FakeProvider,
        info: ProviderInfo,
        likes_unavailable: bool,
        endless_pages: bool,
        expired_stream: bool,
    }

    fn mutant(inner: FakeProvider) -> Mutant {
        let info = inner.info().clone();
        Mutant {
            inner,
            info,
            likes_unavailable: false,
            endless_pages: false,
            expired_stream: false,
        }
    }

    impl Provider for Mutant {
        fn info(&self) -> &ProviderInfo {
            &self.info
        }
        fn login(&self, token: Token) -> Result<Account, ProviderError> {
            self.inner.login(token)
        }
        fn logout(&self) {
            self.inner.logout()
        }
        fn account(&self) -> Result<Account, ProviderError> {
            self.inner.account()
        }
        fn search(&self, query: &str, page: u32) -> Result<Page<Track>, ProviderError> {
            let mut result = self.inner.search(query, page)?;
            if self.endless_pages {
                result.next = Some((page + 1).to_string());
            }
            Ok(result)
        }
        fn search_artists(&self, query: &str, page: u32) -> Result<Page<Artist>, ProviderError> {
            self.inner.search_artists(query, page)
        }
        fn search_albums(&self, query: &str, page: u32) -> Result<Page<Album>, ProviderError> {
            self.inner.search_albums(query, page)
        }
        fn search_playlists(
            &self,
            query: &str,
            page: u32,
        ) -> Result<Page<Playlist>, ProviderError> {
            self.inner.search_playlists(query, page)
        }
        fn create_playlist(&self, title: &str) -> Result<Playlist, ProviderError> {
            self.inner.create_playlist(title)
        }
        fn delete_playlist(&self, id: &TrackId) -> Result<(), ProviderError> {
            self.inner.delete_playlist(id)
        }
        fn playlist_snapshot(&self, id: &TrackId) -> Result<PlaylistSnapshot, ProviderError> {
            self.inner.playlist_snapshot(id)
        }
        fn edit_playlist(
            &self,
            snapshot: &PlaylistSnapshot,
            edit: PlaylistEdit,
        ) -> Result<PlaylistSnapshot, PlaylistEditError> {
            self.inner.edit_playlist(snapshot, edit)
        }
        fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
            self.inner.track(id)
        }
        fn album(&self, id: &TrackId) -> Result<Album, ProviderError> {
            self.inner.album(id)
        }
        fn playlist(&self, id: &TrackId) -> Result<Playlist, ProviderError> {
            self.inner.playlist(id)
        }
        fn playlists(&self, cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
            self.inner.playlists(cursor)
        }
        fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
            if self.likes_unavailable {
                return Err(ProviderError::Unavailable);
            }
            self.inner.likes(cursor)
        }
        fn set_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError> {
            self.inner.set_liked(id, liked)
        }
        fn set_disliked(&self, id: &TrackId, disliked: bool) -> Result<(), ProviderError> {
            self.inner.set_disliked(id, disliked)
        }
        fn artist(&self, id: &TrackId) -> Result<ArtistPage, ProviderError> {
            self.inner.artist(id)
        }
        fn artist_tracks(
            &self,
            id: &TrackId,
            cursor: Option<&str>,
        ) -> Result<Page<Track>, ProviderError> {
            self.inner.artist_tracks(id, cursor)
        }
        fn radio_start(&self, tuning: &crate::RadioTuning) -> Result<RadioSession, RadioError> {
            self.inner.radio_start(tuning)
        }
        fn radio_next(
            &self,
            session: &RadioSession,
            queue: &[crate::RadioQueueItem],
        ) -> Result<crate::RadioBatch, RadioError> {
            self.inner.radio_next(session, queue)
        }
        fn resolve_stream(&self, id: &TrackId, quality: Quality) -> Result<Stream, ProviderError> {
            let mut stream = self.inner.resolve_stream(id, quality)?;
            if self.expired_stream {
                stream.expires_at = SystemTime::now() - Duration::from_secs(60);
            }
            Ok(stream)
        }
    }

    #[test]
    fn fake_provider_passes() {
        let provider = FakeProvider::new("fakea").with_tracks(catalog("fakea"));
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn signed_out_fake_passes_without_login() {
        let provider = FakeProvider::new("fakea")
            .with_tracks(catalog("fakea"))
            .signed_out();
        let violations = run(&provider, &fixtures("fakea", false));
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn bad_provider_id_is_reported() {
        let mut provider = mutant(FakeProvider::new("fakea").with_tracks(catalog("fakea")));
        provider.info.id = "Bad_Id";
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(has(&violations, "info.id"), "{violations:?}");
    }

    #[test]
    fn claimed_but_unavailable_capability_is_reported() {
        let mut provider = mutant(FakeProvider::new("fakea").with_tracks(catalog("fakea")));
        provider.likes_unavailable = true;
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(has(&violations, "capability.likes"), "{violations:?}");
    }

    #[test]
    fn endless_pagination_is_reported() {
        let mut provider = mutant(FakeProvider::new("fakea").with_tracks(catalog("fakea")));
        provider.endless_pages = true;
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(has(&violations, "pagination"), "{violations:?}");
    }

    #[test]
    fn expired_stream_is_reported() {
        let mut provider = mutant(FakeProvider::new("fakea").with_tracks(catalog("fakea")));
        provider.expired_stream = true;
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(has(&violations, "stream.expires_at"), "{violations:?}");
    }

    #[test]
    fn isrc_claim_without_isrc_is_reported() {
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.isrc = true;
        let provider = FakeProvider::new("fakea")
            .with_tracks(catalog("fakea"))
            .with_capabilities(capabilities);
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(has(&violations, "isrc"), "{violations:?}");
    }

    #[test]
    fn isrc_claim_with_isrc_passes() {
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.isrc = true;
        let mut tracks = catalog("fakea");
        tracks[0].isrc = Some("USRC17600001".into());
        let provider = FakeProvider::new("fakea")
            .with_tracks(tracks)
            .with_capabilities(capabilities);
        let violations = run(&provider, &fixtures("fakea", true));
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn check_info_reports_empty_name() {
        let info = ProviderInfo {
            id: "fakea",
            name: " ",
            auth: crate::AuthMethod::None,
            capabilities: Capabilities::none(),
        };
        assert!(has(&check_info(&info), "info.name"));
    }
}
