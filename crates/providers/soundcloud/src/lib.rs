// SPDX-License-Identifier: GPL-3.0-or-later
//! Guest access to freely playable public SoundCloud tracks.
use eagle_provider_sdk::*;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use url::Url;

const API: &str = "https://api-v2.soundcloud.com";
const HOME: &str = "https://soundcloud.com/";
const LIMIT: u32 = 50;
const MAX_SCRIPT: usize = 8 * 1024 * 1024;

pub struct SoundCloud {
    http: Arc<dyn Http>,
    client: Mutex<Option<Token>>,
    session: Mutex<Option<(Token, Account)>>,
    info: ProviderInfo,
}

impl SoundCloud {
    pub fn new(http: Arc<dyn Http>) -> Self {
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.likes_read = true;
        Self {
            http,
            client: Mutex::new(None),
            session: Mutex::new(None),
            info: ProviderInfo {
                id: "soundcloud",
                name: "SoundCloud",
                sign_in: &[SignInMethod::Token {
                    hint: "Токен веб-сеанса",
                    help_url: Some("https://www.music-assistant.io/music-providers/soundcloud/"),
                }],
                capabilities,
            },
        }
    }

    fn bytes(&self, url: Url, limit: usize) -> Result<Vec<u8>, HttpError> {
        let mut req = Request::get(SignedUrl::new(url.into()));
        req.limit = limit;
        Ok(self.http.request(Priority::Metadata, req, None)?.bytes)
    }

    fn discover(&self) -> Result<Token, ProviderError> {
        let url = Url::parse(HOME).map_err(|_| ProviderError::ApiChanged)?;
        let html = self.bytes(url, 2 * 1024 * 1024).map_err(net_error)?;
        let html = std::str::from_utf8(&html).map_err(|_| ProviderError::ApiChanged)?;
        for src in html
            .rsplit("<script")
            .filter_map(|tag| {
                let tag = tag.split('>').next()?;
                let rest = tag.split_once("src=\"")?.1;
                rest.split('"').next()
            })
            .take(32)
        {
            let Ok(url) = Url::parse(src) else {
                continue;
            };
            if url.scheme() != "https"
                || url.host_str() != Some("a-v2.sndcdn.com")
                || url.port().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
                || !url.path().starts_with("/assets/")
            {
                continue;
            }
            let Ok(bytes) = self.bytes(url, MAX_SCRIPT) else {
                continue;
            };
            if let Ok(script) = std::str::from_utf8(&bytes)
                && let Some(client) = extract_client_id(script)
            {
                return Ok(Token::new(client.into()));
            }
        }
        Err(ProviderError::ApiChanged)
    }
    fn client(&self) -> Result<Token, ProviderError> {
        let mut cached = self.client.lock().map_err(|_| ProviderError::ApiChanged)?;
        if let Some(client) = &*cached {
            return Ok(client.clone());
        }
        let client = self.discover()?;
        *cached = Some(client.clone());
        Ok(client)
    }
    fn get(&self, path: &str, params: &[(&str, String)]) -> Result<(Value, bool), ProviderError> {
        let mut url = Url::parse(&format!("{API}{path}")).map_err(|_| ProviderError::ApiChanged)?;
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        // Preserve search parameters across the credential refresh retry.
        self.get_with_params(url)
    }
    fn get_with_params(&self, url: Url) -> Result<(Value, bool), ProviderError> {
        let token = self
            .session
            .lock()
            .map_err(|_| ProviderError::ApiChanged)?
            .as_ref()
            .map(|(token, _)| token.clone());
        self.get_with_token(url, token)
    }
    fn get_with_token(
        &self,
        url: Url,
        token: Option<Token>,
    ) -> Result<(Value, bool), ProviderError> {
        let original_query = url.query().map(str::to_owned);
        let mut url = url;
        if url.scheme() != "https"
            || url.host_str() != Some("api-v2.soundcloud.com")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProviderError::ApiChanged);
        }
        for attempt in 0..2 {
            url.set_query(original_query.as_deref());
            let client = self.client()?;
            url.query_pairs_mut()
                .append_pair("client_id", client.expose());
            let mut req = Request::get(SignedUrl::new(url.clone().into()));
            req.limit = 4 * 1024 * 1024;
            req.token = token.clone();
            let authenticated = req.token.is_some();
            if !authenticated
                && url
                    .query_pairs()
                    .any(|(key, _)| key == "track_authorization")
            {
                return Err(ProviderError::Auth);
            }
            match self.http.request(Priority::Metadata, req, None) {
                Ok(response) => {
                    let value = serde_json::from_slice(&response.bytes)
                        .map_err(|_| ProviderError::ApiChanged)?;
                    return Ok((value, authenticated));
                }
                Err(HttpError::Status(401)) if attempt == 0 => {
                    self.invalidate_client(&client)?;
                }
                Err(e) => return Err(net_error(e)),
            }
        }
        Err(ProviderError::Auth)
    }

    fn invalidate_client(&self, client: &Token) -> Result<(), ProviderError> {
        let mut cached = self.client.lock().map_err(|_| ProviderError::ApiChanged)?;
        if cached
            .as_ref()
            .is_some_and(|old| old.expose() == client.expose())
        {
            *cached = None;
        }
        Ok(())
    }

    fn validate_session(&self, token: &Token) -> Result<Account, ProviderError> {
        for attempt in 0..2 {
            let mut url = Url::parse("https://api-v2.soundcloud.com/me")
                .map_err(|_| ProviderError::ApiChanged)?;
            let client = self.client()?;
            url.query_pairs_mut()
                .append_pair("client_id", client.expose());
            let mut req = Request::get(SignedUrl::new(url.into()));
            req.token = Some(token.clone());
            req.limit = 1024 * 1024;
            let response = match self.http.request(Priority::Metadata, req, None) {
                Ok(response) => response,
                Err(HttpError::Status(401)) if attempt == 0 => {
                    self.invalidate_client(&client)?;
                    continue;
                }
                Err(error) => return Err(net_error(error)),
            };
            let value: Value =
                serde_json::from_slice(&response.bytes).map_err(|_| ProviderError::ApiChanged)?;
            let id = value["id"]
                .as_u64()
                .ok_or(ProviderError::ApiChanged)?
                .to_string();
            return Ok(Account {
                id,
                subscription: false,
                name: value["username"].as_str().map(str::to_owned),
                capabilities: self.info.capabilities,
            });
        }
        Err(ProviderError::Auth)
    }
}
fn extract_client_id(script: &str) -> Option<&str> {
    script.match_indices("client_id").find_map(|(at, _)| {
        let tail = script[at + "client_id".len()..].trim_start();
        let tail = tail.strip_prefix(':')?.trim_start().strip_prefix('"')?;
        let value = tail.split('"').next()?;
        (value.len() == 32 && value.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(value)
    })
}
fn net_error(e: HttpError) -> ProviderError {
    match e {
        HttpError::Status(401) => ProviderError::Auth,
        HttpError::Status(403 | 451) => ProviderError::Unavailable,
        HttpError::Status(404) => ProviderError::NotFound,
        HttpError::Status(429) => ProviderError::RateLimited,
        HttpError::TooLarge => ProviderError::ApiChanged,
        _ => ProviderError::Network,
    }
}
fn id(v: &Value) -> Result<TrackId, ProviderError> {
    let n = v.as_u64().ok_or(ProviderError::ApiChanged)?;
    format!("soundcloud:{n}").parse()
}
fn local(id: &TrackId) -> Result<&str, ProviderError> {
    if id.provider() != "soundcloud" || !id.local().bytes().all(|b| b.is_ascii_digit()) {
        return Err(ProviderError::InvalidId);
    }
    Ok(id.local())
}
fn playable(v: &Value, authenticated: bool) -> bool {
    v["policy"].as_str() == Some("ALLOW")
        && (v["sharing"].as_str() == Some("public")
            || (authenticated && v["sharing"].as_str() == Some("private")))
        && v["streamable"].as_bool() == Some(true)
}
fn safe_permalink(value: Option<&str>) -> Option<String> {
    let url = Url::parse(value?).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some("soundcloud.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.into())
}
fn parse_track(v: &Value, authenticated: bool) -> Result<Track, ProviderError> {
    let user = &v["user"];
    Ok(Track {
        id: id(&v["id"])?,
        title: v["title"].as_str().ok_or(ProviderError::ApiChanged)?.into(),
        artists: vec![Artist {
            id: id(&user["id"])?,
            name: user["username"]
                .as_str()
                .ok_or(ProviderError::ApiChanged)?
                .into(),
            cover: user["avatar_url"].as_str().map(str::to_owned),
        }],
        album: None,
        album_title: None,
        duration_ms: v["duration"].as_u64().unwrap_or(0),
        cover: v["artwork_url"]
            .as_str()
            .or_else(|| user["avatar_url"].as_str())
            .map(str::to_owned),
        availability: if playable(v, authenticated) {
            Availability::Available
        } else {
            Availability::Unknown
        },
        source_url: if v["sharing"].as_str() == Some("private") {
            safe_permalink(user["permalink_url"].as_str())
        } else {
            safe_permalink(v["permalink_url"].as_str())
        },
        isrc: None,
    })
}
fn select_hls(
    v: &Value,
    quality: Quality,
    authenticated: bool,
) -> Result<(&str, u32), ProviderError> {
    if !playable(v, authenticated) {
        return Err(ProviderError::Unavailable);
    }
    let formats = v["media"]["transcodings"]
        .as_array()
        .ok_or(ProviderError::Unavailable)?;
    let pick = |preset: &str| {
        formats.iter().find_map(|item| {
            if item["preset"].as_str() != Some(preset)
                || item["format"]["protocol"].as_str() != Some("hls")
                || item["snipped"].as_bool() != Some(false)
                || (!authenticated && item["quality"].as_str() == Some("hq"))
            {
                return None;
            }
            item["url"].as_str().filter(|s| !s.contains("/preview/"))
        })
    };
    let presets = if quality == Quality::High {
        [("aac_160k", 160), ("aac_96k", 96)]
    } else {
        [("aac_96k", 96), ("aac_160k", 160)]
    };
    presets
        .into_iter()
        .find_map(|(preset, rate)| pick(preset).map(|url| (url, rate)))
        .ok_or(ProviderError::Unavailable)
}
impl Provider for SoundCloud {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn attribution_logo(&self) -> Option<&'static [u8]> {
        Some(include_bytes!("../assets/logo-white.png"))
    }
    fn guest_capabilities(&self) -> Capabilities {
        let mut capabilities = self.info.capabilities;
        capabilities.likes_read = false;
        capabilities
    }
    fn login(&self, token: Token) -> Result<Account, ProviderError> {
        let account = self.validate_session(&token)?;
        *self.session.lock().map_err(|_| ProviderError::ApiChanged)? =
            Some((token, account.clone()));
        Ok(account)
    }
    fn account(&self) -> Result<Account, ProviderError> {
        self.session
            .lock()
            .map_err(|_| ProviderError::ApiChanged)?
            .as_ref()
            .map(|(_, account)| account.clone())
            .ok_or(ProviderError::Auth)
    }
    fn logout(&self) {
        if let Ok(mut session) = self.session.lock() {
            *session = None;
        }
    }
    fn search(&self, query: &str, index: u32) -> Result<Page<Track>, ProviderError> {
        let offset = index.checked_mul(LIMIT).ok_or(ProviderError::ApiChanged)?;
        let (v, authenticated) = self.get(
            "/search/tracks",
            &[
                ("q", query.into()),
                ("limit", LIMIT.to_string()),
                ("offset", offset.to_string()),
            ],
        )?;
        let items = v["collection"]
            .as_array()
            .ok_or(ProviderError::ApiChanged)?
            .iter()
            .map(|v| parse_track(v, authenticated))
            .collect::<Result<Vec<_>, _>>()?;
        let next = if v["next_href"].as_str().is_some() {
            Some(
                offset
                    .checked_add(LIMIT)
                    .ok_or(ProviderError::ApiChanged)?
                    .to_string(),
            )
        } else {
            None
        };
        Ok(Page { items, next })
    }
    fn track(&self, track: &TrackId) -> Result<Track, ProviderError> {
        let (value, authenticated) = self.get(&format!("/tracks/{}", local(track)?), &[])?;
        parse_track(&value, authenticated)
    }
    fn album(&self, _: &TrackId) -> Result<Album, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn playlist(&self, _: &TrackId) -> Result<Playlist, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn playlists(&self, _: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
        let (token, account) = self
            .session
            .lock()
            .map_err(|_| ProviderError::ApiChanged)?
            .clone()
            .ok_or(ProviderError::Auth)?;
        let offset = match cursor {
            Some(value) => value.parse::<u32>().map_err(|_| ProviderError::InvalidId)?,
            None => 0,
        };
        let mut url = Url::parse(&format!("{API}/users/{}/track_likes", account.id))
            .map_err(|_| ProviderError::ApiChanged)?;
        url.query_pairs_mut()
            .append_pair("limit", &LIMIT.to_string())
            .append_pair("offset", &offset.to_string());
        let (value, authenticated) = self.get_with_token(url, Some(token))?;
        if !authenticated {
            return Err(ProviderError::Auth);
        }
        let items = value["collection"]
            .as_array()
            .ok_or(ProviderError::ApiChanged)?
            .iter()
            .map(|entry| parse_track(&entry["track"], true))
            .collect::<Result<Vec<_>, _>>()?;
        let next = if value["next_href"].as_str().is_some() {
            Some(
                offset
                    .checked_add(LIMIT)
                    .ok_or(ProviderError::ApiChanged)?
                    .to_string(),
            )
        } else {
            None
        };
        Ok(Page { items, next })
    }
    fn set_liked(&self, _: &TrackId, _: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn set_disliked(&self, _: &TrackId, _: bool) -> Result<(), ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn resolve_stream(&self, track: &TrackId, quality: Quality) -> Result<Stream, ProviderError> {
        let (value, authenticated) = self.get(&format!("/tracks/{}", local(track)?), &[])?;
        let (transcoding, bitrate) = select_hls(&value, quality, authenticated)?;
        let mut url = Url::parse(transcoding).map_err(|_| ProviderError::ApiChanged)?;
        if url.query().is_some() || !url.path().starts_with("/media/") {
            return Err(ProviderError::ApiChanged);
        }
        if authenticated && let Some(authorization) = value["track_authorization"].as_str() {
            url.query_pairs_mut()
                .append_pair("track_authorization", authorization);
        }
        let (response, _) = self.get_with_params(url)?;
        let stream = response["url"].as_str().ok_or(ProviderError::ApiChanged)?;
        let url = Url::parse(stream).map_err(|_| ProviderError::ApiChanged)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path().contains("/preview/")
        {
            return Err(ProviderError::ApiChanged);
        }
        Ok(Stream {
            url: SignedUrl::new(stream.into()),
            codec: "hls-aac".into(),
            bitrate_kbps: Some(bitrate),
            expires_at: SystemTime::now() + Duration::from_secs(300),
            encryption: Encryption::None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_provider_sdk::testkit::conformance::{self, Fixtures};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn track() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/public-track.json")).unwrap()
    }
    struct Fake {
        requests: Mutex<Vec<String>>,
        invalid_once: AtomicUsize,
        me_invalid_once: AtomicUsize,
    }
    impl Http for Fake {
        fn request(
            &self,
            _: Priority,
            req: Request,
            _: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
        ) -> Result<Response, HttpError> {
            let url = req.url.expose().to_owned();
            self.requests.lock().unwrap().push(url.clone());
            if let Some(token) = &req.token {
                assert!(url.starts_with("https://api-v2.soundcloud.com/"));
                assert!(url.contains("/me?") || token.expose() == "valid");
            }
            let bytes = if url.starts_with("https://api-v2.soundcloud.com/me?") {
                if req.token.as_ref().map(Token::expose) != Some("valid")
                    || self.me_invalid_once.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return Err(HttpError::Status(401));
                }
                br#"{"id":123}"#.to_vec()
            } else if url == HOME {
                b"<script src=\"https://a-v2.sndcdn.com/assets/app.js\"></script>".to_vec()
            } else if url.starts_with("https://a-v2.sndcdn.com/assets/") {
                b"client_id: \"0123456789abcdefghijklmnopqrstuv\"".to_vec()
            } else if url.contains("/search/tracks?") {
                if self.invalid_once.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(HttpError::Status(401));
                }
                serde_json::to_vec(&serde_json::json!({"collection":[track()], "next_href":null}))
                    .unwrap()
            } else if url.contains("/tracks/12?") {
                serde_json::to_vec(&track()).unwrap()
            } else if url.contains("/tracks/13?") {
                let mut private = track();
                private["id"] = 13.into();
                private["sharing"] = "private".into();
                private["track_authorization"] = "fixture-secret".into();
                serde_json::to_vec(&private).unwrap()
            } else if url.contains("/users/123/track_likes?") {
                assert_eq!(req.token.as_ref().map(Token::expose), Some("valid"));
                let mut private = track();
                private["id"] = 13.into();
                private["sharing"] = "private".into();
                if url.contains("offset=50") {
                    serde_json::to_vec(
                        &serde_json::json!({"collection": [{"track": private}], "next_href": null}),
                    )
                    .unwrap()
                } else {
                    serde_json::to_vec(&serde_json::json!({"collection": [{"track": track()}], "next_href": "next"})).unwrap()
                }
            } else if url.contains("/media/example?") {
                if req.token.is_some() {
                    assert!(url.contains("track_authorization=fixture-secret"));
                }
                br#"{"url":"https://cdn.example/playlist.m3u8?signature=fixture"}"#.to_vec()
            } else {
                return Err(HttpError::Status(404));
            };
            Ok(Response {
                status: 200,
                bytes,
                content_range: None,
                etag: None,
            })
        }
    }
    #[test]
    fn guest_conforms_and_refreshes_public_client_after_unauthorized() {
        let fake = Arc::new(Fake {
            requests: Mutex::new(Vec::new()),
            invalid_once: AtomicUsize::new(0),
            me_invalid_once: AtomicUsize::new(1),
        });
        let provider = SoundCloud::new(fake.clone());
        assert!(matches!(
            provider.info().sign_in.first(),
            Some(SignInMethod::Token { .. })
        ));
        let fixtures = Fixtures {
            known_track: "soundcloud:12".parse().unwrap(),
            query: "fixture".into(),
            artist: None,
            signed_in: true,
        };
        let violations = conformance::run(&provider, &fixtures);
        assert!(violations.is_empty(), "{violations:?}");
        let requests = fake.requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|url| url.as_str() == HOME).count(),
            2
        );
        let searches: Vec<_> = requests
            .iter()
            .filter(|url| url.contains("/search/tracks?"))
            .collect();
        assert!(searches.len() >= 2);
        assert!(searches[0].contains("q=fixture"));
        assert!(searches[1].contains("q=fixture"));
        drop(requests);
        assert!(matches!(provider.likes(None), Err(ProviderError::Auth)));
        assert!(matches!(
            provider.login(Token::new("invalid".into())),
            Err(ProviderError::Auth)
        ));
        assert_eq!(
            provider.login(Token::new("valid".into())).unwrap().id,
            "123"
        );
        assert_eq!(provider.account().unwrap().id, "123");
        assert!(matches!(
            provider.login(Token::new("invalid".into())),
            Err(ProviderError::Auth)
        ));
        assert_eq!(provider.account().unwrap().id, "123");
        let private_id: TrackId = "soundcloud:13".parse().unwrap();
        let prior_requests = fake.requests.lock().unwrap().len();
        let hostile =
            Url::parse("https://example.invalid/media/a?track_authorization=fixture-secret")
                .unwrap();
        assert!(matches!(
            provider.get_with_params(hostile),
            Err(ProviderError::ApiChanged)
        ));
        assert_eq!(fake.requests.lock().unwrap().len(), prior_requests);
        assert!(
            !format!(
                "{:?}",
                SignedUrl::new(
                    "https://api-v2.soundcloud.com/media/a?track_authorization=fixture-secret"
                        .into()
                )
            )
            .contains("fixture-secret")
        );
        let first = provider.likes(None).unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.next.as_deref(), Some("50"));
        let second = provider.likes(first.next.as_deref()).unwrap();
        assert_eq!(second.items[0].id, private_id);
        assert!(second.next.is_none());
        assert!(matches!(
            provider.likes(Some("invalid")),
            Err(ProviderError::InvalidId)
        ));
        assert_eq!(provider.search("Fixture", 0).unwrap().items.len(), 1);
        assert!(provider.resolve_stream(&private_id, Quality::High).is_ok());
        provider.logout();
        assert!(matches!(provider.account(), Err(ProviderError::Auth)));
        assert!(
            !provider
                .search("Fixture", 0)
                .unwrap()
                .items
                .iter()
                .any(|t| t.id == private_id)
        );
        assert!(matches!(
            provider.resolve_stream(&private_id, Quality::High),
            Err(ProviderError::Unavailable)
        ));
    }
    #[test]
    fn likes_retry_keeps_account_token_after_logout() {
        use std::sync::Barrier;

        struct BlockingFake {
            inner: Fake,
            reached: Barrier,
            resume: Barrier,
            tokens: Mutex<Vec<Option<String>>>,
        }
        impl Http for BlockingFake {
            fn request(
                &self,
                priority: Priority,
                req: Request,
                refresh: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
            ) -> Result<Response, HttpError> {
                if req.url.expose().contains("/users/123/track_likes?") {
                    let mut tokens = self.tokens.lock().unwrap();
                    tokens.push(req.token.as_ref().map(|token| token.expose().to_owned()));
                    let first = tokens.len() == 1;
                    drop(tokens);
                    if first {
                        self.reached.wait();
                        self.resume.wait();
                        return Err(HttpError::Status(401));
                    }
                }
                self.inner.request(priority, req, refresh)
            }
        }
        let fake = Arc::new(BlockingFake {
            inner: Fake {
                requests: Mutex::new(Vec::new()),
                invalid_once: AtomicUsize::new(0),
                me_invalid_once: AtomicUsize::new(1),
            },
            reached: Barrier::new(2),
            resume: Barrier::new(2),
            tokens: Mutex::new(Vec::new()),
        });
        let provider = SoundCloud::new(fake.clone());
        provider.login(Token::new("valid".into())).unwrap();
        std::thread::scope(|scope| {
            let request = scope.spawn(|| provider.likes(None));
            fake.reached.wait();
            provider.logout();
            fake.resume.wait();
            assert_eq!(request.join().unwrap().unwrap().items.len(), 1);
        });
        assert_eq!(
            *fake.tokens.lock().unwrap(),
            vec![Some("valid".into()), Some("valid".into())]
        );
    }
    #[test]
    fn login_refreshes_expired_client_id() {
        let fake = Arc::new(Fake {
            requests: Mutex::new(Vec::new()),
            invalid_once: AtomicUsize::new(0),
            me_invalid_once: AtomicUsize::new(0),
        });
        let provider = SoundCloud::new(fake.clone());
        assert_eq!(
            provider.login(Token::new("valid".into())).unwrap().id,
            "123"
        );
        assert_eq!(
            fake.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.as_str() == HOME)
                .count(),
            2
        );
    }
    #[test]
    fn authenticated_search_refreshes_expired_client_id() {
        let fake = Arc::new(Fake {
            requests: Mutex::new(Vec::new()),
            invalid_once: AtomicUsize::new(0),
            me_invalid_once: AtomicUsize::new(1),
        });
        let provider = SoundCloud::new(fake.clone());
        provider.login(Token::new("valid".into())).unwrap();
        assert_eq!(provider.search("fixture", 0).unwrap().items.len(), 1);
        let requests = fake.requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|url| url.as_str() == HOME).count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|url| url.contains("/search/tracks?"))
                .count(),
            2
        );
    }
    #[test]
    fn attribution_uses_public_track_or_private_uploader_without_secret_links() {
        let mut value = track();
        value["permalink_url"] = "https://soundcloud.com/artist/song".into();
        value["user"]["permalink_url"] = "https://soundcloud.com/artist".into();
        assert_eq!(
            parse_track(&value, false).unwrap().source_url.as_deref(),
            Some("https://soundcloud.com/artist/song")
        );
        value["sharing"] = "private".into();
        value["permalink_url"] = "https://soundcloud.com/artist/song?secret_token=secret".into();
        assert_eq!(
            parse_track(&value, true).unwrap().source_url.as_deref(),
            Some("https://soundcloud.com/artist")
        );
        for link in [
            "https://evil.example/artist",
            "http://soundcloud.com/artist",
            "https://soundcloud.com/artist?secret=x",
            "https://user@soundcloud.com/artist",
        ] {
            assert!(safe_permalink(Some(link)).is_none());
        }
    }
    #[test]
    fn only_public_full_unencrypted_non_premium_formats_are_selected() {
        let mut value = track();
        assert_eq!(select_hls(&value, Quality::High, false).unwrap().1, 96);
        for policy in ["BLOCK", "SNIP", "MONETIZE"] {
            value["policy"] = policy.into();
            assert!(select_hls(&value, Quality::High, false).is_err());
        }
        value = track();
        value["sharing"] = "private".into();
        assert!(select_hls(&value, Quality::High, false).is_err());
        assert!(select_hls(&value, Quality::High, true).is_ok());
        value = track();
        value["media"]["transcodings"][0]["snipped"] = true.into();
        assert!(select_hls(&value, Quality::High, false).is_err());
        value = track();
        value["media"]["transcodings"][0]["quality"] = "hq".into();
        assert!(select_hls(&value, Quality::High, false).is_err());
        assert!(select_hls(&value, Quality::High, true).is_ok());
        value = track();
        value["media"]["transcodings"][0]["format"]["protocol"] = "encrypted-hls".into();
        assert!(select_hls(&value, Quality::High, false).is_err());
        assert!(extract_client_id("client_id: \"too-short\"").is_none());
    }
}
