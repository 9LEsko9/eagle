// SPDX-License-Identifier: GPL-3.0-or-later
//! Blocking Yandex Music adapter. Secrets never enter SQLite or error text.
pub mod contracts;

use eagle_provider_sdk::*;
use serde_json::{Map, Value, json};
use std::{
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime},
};
const API: &str = "https://api.music.yandex.net";
const OAUTH: &str = "https://oauth.yandex.ru";
/// Verification page the device flow falls back to when the answer names none.
const DEVICE_PAGE: &str = "https://ya.ru/device";
/// Public identifiers of the Yandex Music client; the device flow needs both.
const OAUTH_CLIENT_ID: &str = "23cabbbdc6cd418abb4b39c32c41195d";
const OAUTH_CLIENT_SECRET: &str = "53bc75238f0c4d08a118e51fe9203300";
/// Used when the OAuth answer omits `interval` or `expires_in`.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_CODE_LIFETIME: Duration = Duration::from_secs(300);
pub struct Yandex {
    net: Arc<dyn Http>,
    #[cfg(test)]
    api_base: Option<String>,
    #[cfg(test)]
    oauth_base: Option<String>,
    token: RwLock<Option<Token>>,
    liked_ids: Mutex<Option<Vec<String>>>,
    /// Account uid, cached so likes and lists cost one request instead of two.
    uid: Mutex<Option<String>>,
    info: ProviderInfo,
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
fn uncertain_write(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Network | ProviderError::ApiChanged => ProviderError::Uncertain,
        other => other,
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    v[key].as_str().ok_or(ProviderError::ApiChanged)
}
fn ident(v: &Value) -> Result<String, ProviderError> {
    if let Some(s) = v.as_str() {
        Ok(s.into())
    } else if let Some(n) = v.as_u64() {
        Ok(n.to_string())
    } else {
        Err(ProviderError::ApiChanged)
    }
}
fn id(v: &Value) -> Result<TrackId, ProviderError> {
    format!("yandex:{}", ident(v)?).parse()
}
fn artists(v: &Value) -> Result<Vec<Artist>, ProviderError> {
    v.as_array()
        .ok_or(ProviderError::ApiChanged)?
        .iter()
        .map(|a| {
            Ok(Artist {
                id: id(&a["id"])?,
                name: text(a, "name")?.into(),
                cover: artist_cover(a),
            })
        })
        .collect()
}
fn artist_cover(a: &Value) -> Option<String> {
    a["cover"]["uri"]
        .as_str()
        .or_else(|| a["ogImage"].as_str())
        .map(str::to_owned)
}
fn parse_artist(a: &Value) -> Result<Artist, ProviderError> {
    let a = a.get("artist").unwrap_or(a);
    Ok(Artist {
        id: id(&a["id"])?,
        name: text(a, "name")?.into(),
        cover: artist_cover(a),
    })
}
/// Album header without tracks (lists, likes, discography).
fn parse_album(v: &Value) -> Result<Album, ProviderError> {
    let v = v.get("album").filter(|a| a.is_object()).unwrap_or(v);
    Ok(Album {
        id: id(&v["id"])?,
        title: text(v, "title")?.into(),
        artists: if v["artists"].is_array() {
            artists(&v["artists"])?
        } else {
            Vec::new()
        },
        tracks: Vec::new(),
        cover: v["coverUri"].as_str().map(str::to_owned),
        year: v["year"].as_u64().and_then(|y| u32::try_from(y).ok()),
    })
}
pub fn parse_track(v: &Value) -> Result<Track, ProviderError> {
    Ok(Track {
        id: id(&v["id"])?,
        title: text(v, "title")?.into(),
        artists: artists(&v["artists"])?,
        album: v["albums"]
            .as_array()
            .and_then(|a| a.first())
            .map(|a| id(&a["id"]))
            .transpose()?,
        album_title: v["albums"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a["title"].as_str())
            .map(str::to_owned),
        duration_ms: v["durationMs"].as_u64().unwrap_or(0),
        cover: v["coverUri"].as_str().map(str::to_owned),
        availability: if v["available"].as_bool() == Some(true) {
            Availability::Available
        } else {
            Availability::Unknown
        },
        isrc: None,
    })
}
fn tracks(v: &Value) -> Result<Vec<Track>, ProviderError> {
    v.as_array()
        .ok_or(ProviderError::ApiChanged)?
        .iter()
        .map(|t| parse_track(t.get("track").unwrap_or(t)))
        .collect()
}
fn valid_session(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn radio_batch(value: &Value) -> Result<RadioBatch, RadioError> {
    let id = match value.get("batchId") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        _ => return Err(RadioError::Uncertain),
    };
    Ok(RadioBatch {
        id,
        tracks: tracks(&value["sequence"]).map_err(|_| RadioError::Uncertain)?,
    })
}
/// The rotor `settingMoodEnergy` value for a mood direction of `RadioTuning::mood_direction`:
/// bright and cheerful are `fun`, energetic and edgy `active`, gloomy and sad `sad`, quiet and
/// calm `calm`. The service has no finer scale, so the direction is folded into four values.
fn mood_setting(direction: usize) -> &'static str {
    match direction {
        0 | 1 => "fun",
        2 | 3 => "active",
        4 | 5 => "sad",
        _ => "calm",
    }
}
/// The rotor seeds of a wave: `user:onyourwave` (or `track:<id>` for a seed track) plus the
/// `settingMoodEnergy` and `settingDiversity` settings that the tuning asks for. The default
/// tuning sends no settings, which is the plain "My wave".
fn radio_seeds(tuning: &RadioTuning) -> Result<Vec<String>, ProviderError> {
    let tuning = tuning.normalized();
    let mut seeds = vec![match &tuning.seed {
        Some(seed) => {
            if seed.provider() != "yandex"
                || seed.local().is_empty()
                || !seed.local().bytes().all(|b| b.is_ascii_digit())
            {
                return Err(ProviderError::InvalidId);
            }
            format!("track:{}", seed.local())
        }
        None => "user:onyourwave".to_owned(),
    }];
    if let Some(direction) = tuning.mood_direction() {
        seeds.push(format!("settingMoodEnergy:{}", mood_setting(direction)));
    }
    match tuning.novelty_band() {
        NoveltyBand::Familiar => seeds.push("settingDiversity:favorite".into()),
        NoveltyBand::New => seeds.push("settingDiversity:discover".into()),
        NoveltyBand::Balanced => {}
    }
    Ok(seeds)
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
/// Stands in for the account's user id, in paths and bodies alike.
pub const STUB_UID: &str = "1";
/// Stands in for every like, playlist and account date in a recording.
const STUB_TIME: &str = "2000-01-01T00:00:00+00:00";
/// Stands in for avatars and playlist covers.
const STUB_AVATAR: &str = "avatars.yandex.net/stub/%%";
/// Stands in for a signed download link; `ts` is the only parameter a recording keeps.
const STUB_DOWNLOAD_URL: &str = "https://api.music.yandex.net/get-mp3/stub?sign=stub&ts=0";
/// Stands in for the XML of a download answer. `direct_link` must still parse it.
const STUB_DOWNLOAD_XML: &str = "<download-info><host>stub.invalid</host><path>/stub</path><ts>0</ts><region>-1</region><s>stub</s></download-info>";
/// Likes keep this many items in a recording.
const LIKES_LIMIT: usize = 3;
/// Playlists keep this many items in a recording.
const PLAYLISTS_LIMIT: usize = 2;
/// A batch lookup of tracks keeps this many items in a recording.
const TRACKS_LIMIT: usize = 3;
/// Each kind of search result keeps this many items in a recording.
const SEARCH_LIMIT: usize = 5;
/// Account fields and the stand-ins that replace them in recordings.
const ACCOUNT_FIELDS: &[(&str, &str)] = &[
    ("uid", STUB_UID),
    ("login", "stub-login"),
    ("email", "stub@example.invalid"),
    ("displayName", "Stub User"),
    ("fullName", "Stub User"),
    ("firstName", "Stub"),
    ("lastName", "User"),
    ("secondName", "User"),
    ("birthday", "2000-01-01"),
    ("defaultEmail", "stub@example.invalid"),
    ("userhash", "stub-hash"),
    ("orderId", "stub-order"),
    ("phone", "+70000000000"),
    ("phones", "+70000000000"),
    ("phoneNumber", "+70000000000"),
    ("playlistUuid", "stub-uuid"),
    ("timestamp", STUB_TIME),
    ("created", STUB_TIME),
    ("modified", STUB_TIME),
];

/// Removes account data from one recorded answer before it is written: the user id in the path
/// and the body, login, e-mail, names, phones, dates, avatars and signed download links. Likes,
/// playlists, search results and batch lookups keep a few items only. Applying it twice changes
/// nothing.
///
/// A download answer (`/get-mp3/`) is XML: its body arrives as `Value::Null`, and the scrub replaces
/// it with a string holding the stub XML, which the recorder stores in place of the real bytes.
pub fn scrub_recording(path: &mut String, query: &mut String, body: &mut Value) {
    if path.contains("/get-mp3/") {
        *path = "/get-mp3/stub".to_owned();
        *query = "ts=0".to_owned();
        *body = Value::String(STUB_DOWNLOAD_XML.to_owned());
        return;
    }
    let mut previous = "";
    let segments: Vec<String> = path
        .split('/')
        .map(|segment| {
            let is_uid = previous == "users" && is_digits(segment);
            previous = segment;
            if is_uid {
                STUB_UID.to_owned()
            } else {
                segment.to_owned()
            }
        })
        .collect();
    *path = segments.join("/");
    if path.contains("/playlists")
        && let Some(items) = body.get_mut("result").and_then(Value::as_array_mut)
    {
        items.truncate(PLAYLISTS_LIMIT);
        items.iter_mut().for_each(scrub_playlist);
    }
    if path == "/tracks"
        && let Some(items) = body.get_mut("result").and_then(Value::as_array_mut)
    {
        items.truncate(TRACKS_LIMIT);
    }
    if path == "/search"
        && let Some(kinds) = body.get_mut("result").and_then(Value::as_object_mut)
    {
        for kind in kinds.values_mut() {
            if let Some(items) = kind.as_array_mut() {
                items.truncate(SEARCH_LIMIT);
            } else if let Some(items) = kind.get_mut("results").and_then(Value::as_array_mut) {
                items.truncate(SEARCH_LIMIT);
            }
        }
    }
    let limit = if path.contains("/likes") {
        Some(LIKES_LIMIT)
    } else if path.contains("/playlists") {
        Some(PLAYLISTS_LIMIT)
    } else {
        None
    };
    scrub_value(body, limit);
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Playlist titles, covers, custom waves and owners are the user's own content; the generic
/// rules below only catch account fields and avatars.
fn scrub_playlist(playlist: &mut Value) {
    let Some(map) = playlist.as_object_mut() else {
        return;
    };
    replace_field(map, "title", "Stub playlist");
    replace_field(map, "description", "stub");
    replace_field(map, "ogImage", STUB_AVATAR);
    if let Some(Value::Object(owner)) = map.get_mut("owner") {
        replace_field(owner, "name", "Stub User");
    }
    if let Some(Value::Object(cover)) = map.get_mut("cover")
        && cover.contains_key("itemsUri")
    {
        cover.insert("itemsUri".to_owned(), json!([STUB_AVATAR]));
    }
    if let Some(Value::Object(wave)) = map.get_mut("customWave") {
        for item in wave.values_mut() {
            if item.is_string() {
                *item = json!("stub");
            }
        }
    }
}

fn replace_field(map: &mut Map<String, Value>, field: &str, stub: &str) {
    if let Some(item) = map.get_mut(field) {
        *item = json!(stub);
    }
}

fn scrub_value(value: &mut Value, limit: Option<usize>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if let Some((_, stub)) = ACCOUNT_FIELDS
                    .iter()
                    .find(|(field, _)| *field == key.as_str())
                {
                    *item = json!(stub);
                } else if key.starts_with("avatar") && item.is_string() {
                    *item = json!(STUB_AVATAR);
                } else {
                    scrub_value(item, limit);
                }
            }
        }
        Value::Array(items) => {
            if let Some(max) = limit {
                items.truncate(max);
            }
            items.iter_mut().for_each(|item| scrub_value(item, limit));
        }
        Value::String(text) => {
            if text.contains("avatars.yandex.net/get-music-content/") {
                *text = STUB_AVATAR.to_owned();
            } else if text.contains("/get-mp3/") {
                *text = STUB_DOWNLOAD_URL.to_owned();
            }
        }
        _ => {}
    }
}

impl Yandex {
    pub fn new(net: Arc<dyn Http>) -> Self {
        let mut capabilities = Capabilities::none();
        capabilities.search_tracks = true;
        capabilities.search_artists = true;
        capabilities.search_albums = true;
        capabilities.search_playlists = true;
        capabilities.likes = true;
        capabilities.dislikes = true;
        capabilities.liked_albums = true;
        capabilities.liked_artists = true;
        capabilities.playlists_read = true;
        capabilities.playlists_edit = true;
        capabilities.artist_page = true;
        capabilities.radio = true;
        capabilities.radio_tuning = true;
        capabilities.radio_seed = true;
        capabilities.offline = true;
        Self {
            info: ProviderInfo {
                id: "yandex",
                name: "Яндекс Музыка",
                sign_in: &[
                    SignInMethod::DeviceCode,
                    SignInMethod::Token {
                        hint: "OAuth-токен Яндекс Музыки",
                        help_url: None,
                    },
                ],
                capabilities,
            },
            net,
            #[cfg(test)]
            api_base: None,
            #[cfg(test)]
            oauth_base: None,
            token: RwLock::new(None),
            liked_ids: Mutex::new(None),
            uid: Mutex::new(None),
        }
    }
    fn token(&self) -> Result<Token, ProviderError> {
        self.token
            .read()
            .map_err(|_| ProviderError::Auth)?
            .clone()
            .ok_or(ProviderError::Auth)
    }
    fn api(&self, path: &str, form: Option<Vec<(String, String)>>) -> Result<Value, ProviderError> {
        self.api_at(path, form, Priority::Metadata)
    }
    fn radio_post(&self, path: &str, body: Value) -> Result<Value, RadioError> {
        #[cfg(test)]
        let base = self.api_base.as_deref().unwrap_or(API);
        #[cfg(not(test))]
        let base = API;
        let mut request = Request::get(SignedUrl::new(format!("{base}{path}")));
        request.token = Some(self.token()?);
        request.method = "POST";
        request
            .headers
            .push(("Content-Type".into(), "application/json".into()));
        request.headers.push((
            "X-Yandex-Music-Client".into(),
            "YandexMusicAndroid/24023621".into(),
        ));
        request.body = Some(serde_json::to_vec(&body).map_err(|_| RadioError::Uncertain)?);
        let response = self
            .net
            .request(Priority::Metadata, request, None)
            .map_err(|_| RadioError::Uncertain)?;
        let value: Value =
            serde_json::from_slice(&response.bytes).map_err(|_| RadioError::Uncertain)?;
        if value.get("error").is_some() {
            return Err(RadioError::Uncertain);
        }
        value.get("result").cloned().ok_or(RadioError::Uncertain)
    }
    fn api_at(
        &self,
        path: &str,
        form: Option<Vec<(String, String)>>,
        priority: Priority,
    ) -> Result<Value, ProviderError> {
        #[cfg(test)]
        let base = self.api_base.as_deref().unwrap_or(API);
        #[cfg(not(test))]
        let base = API;
        let mut request = Request::get(SignedUrl::new(format!("{base}{path}")));
        request.token = Some(self.token()?);
        request.headers.push((
            "X-Yandex-Music-Client".into(),
            "YandexMusicAndroid/24023621".into(),
        ));
        if let Some(form) = form {
            request.method = "POST";
            request.form = form;
        }
        let response = self
            .net
            .request(priority, request, None)
            .map_err(net_error)?;
        let json: Value =
            serde_json::from_slice(&response.bytes).map_err(|_| ProviderError::ApiChanged)?;
        if json.get("error").is_some() {
            return Err(ProviderError::ApiChanged);
        }
        json.get("result").cloned().ok_or(ProviderError::ApiChanged)
    }
    /// POSTs a form to the OAuth server. A 4xx answer is returned with its body, because
    /// that is where the server says "not confirmed yet" or "code expired".
    fn oauth_post(
        &self,
        path: &str,
        form: Vec<(String, String)>,
    ) -> Result<(u16, Value), ProviderError> {
        #[cfg(test)]
        let base = self.oauth_base.as_deref().unwrap_or(OAUTH);
        #[cfg(not(test))]
        let base = OAUTH;
        let mut request = Request::get(SignedUrl::new(format!("{base}{path}")));
        request.method = "POST";
        request.form = form;
        request.error_body = true;
        let response = self
            .net
            .request(Priority::Metadata, request, None)
            .map_err(net_error)?;
        if response.status == 429 {
            return Err(ProviderError::RateLimited);
        }
        let json =
            serde_json::from_slice(&response.bytes).map_err(|_| ProviderError::ApiChanged)?;
        Ok((response.status, json))
    }
    fn device_code_start(&self) -> Result<SignInStep, ProviderError> {
        let (status, v) = self.oauth_post(
            "/device/code",
            vec![("client_id".into(), OAUTH_CLIENT_ID.into())],
        )?;
        if status != 200 {
            return Err(ProviderError::ApiChanged);
        }
        let device_code = text(&v, "device_code")?;
        let user_code = text(&v, "user_code")?;
        if device_code.is_empty() || user_code.is_empty() {
            return Err(ProviderError::ApiChanged);
        }
        let seconds = |key: &str, default: Duration| {
            v[key]
                .as_u64()
                .filter(|seconds| *seconds > 0)
                .map_or(default, Duration::from_secs)
        };
        Ok(SignInStep::ShowCode {
            code: user_code.into(),
            url: v["verification_url"]
                .as_str()
                .filter(|url| url.starts_with("https://"))
                .unwrap_or(DEVICE_PAGE)
                .into(),
            interval: seconds("interval", DEFAULT_POLL_INTERVAL),
            expires_in: seconds("expires_in", DEFAULT_CODE_LIFETIME),
            flow: SignInFlow(Token::new(device_code.into())),
        })
    }
    fn device_code_poll(&self, flow: &SignInFlow) -> Result<SignInStep, ProviderError> {
        let (status, v) = self.oauth_post(
            "/token",
            vec![
                ("grant_type".into(), "device_code".into()),
                ("code".into(), flow.0.expose().into()),
                ("client_id".into(), OAUTH_CLIENT_ID.into()),
                ("client_secret".into(), OAUTH_CLIENT_SECRET.into()),
            ],
        )?;
        if status == 200 {
            let token = text(&v, "access_token")?;
            if token.is_empty() {
                return Err(ProviderError::ApiChanged);
            }
            return Ok(SignInStep::Done(Token::new(token.into())));
        }
        match v["error"].as_str() {
            Some("authorization_pending") => Ok(SignInStep::Pending),
            // The host widens the interval on `RateLimited` and keeps polling.
            Some("slow_down") => Err(ProviderError::RateLimited),
            Some("expired_token") => Ok(SignInStep::Expired),
            Some("invalid_grant")
                if v["error_description"]
                    .as_str()
                    .is_some_and(|text| text.to_lowercase().contains("expire")) =>
            {
                Ok(SignInStep::Expired)
            }
            Some(_) => Err(ProviderError::Auth),
            None => Err(ProviderError::ApiChanged),
        }
    }
    fn uid(&self) -> Result<String, ProviderError> {
        self.cached_uid(|| Ok(self.account()?.id))
    }
    fn cached_uid(
        &self,
        fetch: impl FnOnce() -> Result<String, ProviderError>,
    ) -> Result<String, ProviderError> {
        let mut cached = self.uid.lock().map_err(|_| ProviderError::Auth)?;
        if let Some(uid) = cached.as_ref() {
            return Ok(uid.clone());
        }
        let uid = fetch()?;
        *cached = Some(uid.clone());
        Ok(uid)
    }
    fn set_token(&self, token: Token) -> Result<(), ProviderError> {
        let mut uid = self.uid.lock().map_err(|_| ProviderError::Auth)?;
        *self.token.write().map_err(|_| ProviderError::Auth)? = Some(token);
        *uid = None;
        *self.liked_ids.lock().map_err(|_| ProviderError::Auth)? = None;
        Ok(())
    }
    fn set_collection_liked(
        &self,
        kind: &str,
        id: &TrackId,
        liked: bool,
    ) -> Result<(), ProviderError> {
        let uid = self.uid()?;
        let action = if liked { "add-multiple" } else { "remove" };
        self.api(
            &format!("/users/{uid}/likes/{kind}s/{action}"),
            Some(vec![(format!("{kind}-ids"), self.local(id)?.into())]),
        )?;
        Ok(())
    }
    fn local<'a>(&self, id: &'a TrackId) -> Result<&'a str, ProviderError> {
        if id.provider() != "yandex" {
            return Err(ProviderError::InvalidId);
        }
        Ok(id.local())
    }
    fn search_section<T>(
        &self,
        query: &str,
        page: u32,
        kind: &str,
        section: &str,
        parse: impl Fn(&Value) -> Result<T, ProviderError>,
    ) -> Result<Page<T>, ProviderError> {
        let result = self.api(
            &format!(
                "/search?type={kind}&text={}&page={page}&nocorrect=true",
                encode(query)
            ),
            None,
        )?;
        let part = &result[section];
        if part.is_null() {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        }
        let items = part["results"]
            .as_array()
            .ok_or(ProviderError::ApiChanged)?
            .iter()
            .map(parse)
            .collect::<Result<Vec<_>, _>>()?;
        let per = part["perPage"]
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or(ProviderError::ApiChanged)?;
        let total = part["total"].as_u64().ok_or(ProviderError::ApiChanged)?;
        let next_page = page.checked_add(1);
        let next = next_page
            .filter(|next| u64::from(*next).saturating_mul(per) < total)
            .map(|next| next.to_string());
        Ok(Page { items, next })
    }
    fn parse_playlist(&self, v: &Value) -> Result<Playlist, ProviderError> {
        let owner = ident(&v["owner"]["uid"])?;
        let kind = ident(&v["kind"])?;
        Ok(Playlist {
            id: format!("yandex:{owner}_{kind}").parse()?,
            title: text(v, "title")?.into(),
            cover: v["cover"]["uri"]
                .as_str()
                .or_else(|| v["coverUri"].as_str())
                .or_else(|| v["ogImage"].as_str())
                .map(str::to_owned),
            tracks: if let Some(entries) = v["tracks"].as_array() {
                entries
                    .iter()
                    .map(|entry| {
                        let mut track = parse_track(entry.get("track").unwrap_or(entry))?;
                        if entry.get("track").is_some() {
                            track.album = entry
                                .get("albumId")
                                .filter(|v| !v.is_null())
                                .map(id)
                                .transpose()?;
                            track.album_title = track.album.as_ref().and_then(|album| {
                                entry["track"]["albums"]
                                    .as_array()?
                                    .iter()
                                    .find_map(|value| {
                                        (id(&value["id"]).ok().as_ref() == Some(album))
                                            .then(|| value["title"].as_str().map(str::to_owned))
                                            .flatten()
                                    })
                            });
                        }
                        Ok(track)
                    })
                    .collect::<Result<Vec<_>, ProviderError>>()?
            } else {
                vec![]
            },
        })
    }
}
impl Provider for Yandex {
    fn radio_start(&self, tuning: &RadioTuning) -> Result<RadioSession, RadioError> {
        let seeds = radio_seeds(tuning)?;
        let value = self.radio_post(
            "/rotor/session/new",
            serde_json::json!({"seeds": seeds, "includeTracksInResponse": true, "incognito": true}),
        )?;
        let id = value["radioSessionId"]
            .as_str()
            .filter(|s| valid_session(s))
            .ok_or(RadioError::Uncertain)?
            .to_owned();
        Ok(RadioSession {
            id,
            batch: radio_batch(&value)?,
        })
    }
    fn radio_next(
        &self,
        session: &RadioSession,
        queue: &[RadioQueueItem],
    ) -> Result<RadioBatch, RadioError> {
        if !valid_session(&session.id) {
            return Err(ProviderError::InvalidId.into());
        }
        let queue = queue
            .iter()
            .map(|item| {
                let track = self.local(&item.track)?;
                if !track.bytes().all(|b| b.is_ascii_digit()) || track.is_empty() {
                    return Err(ProviderError::InvalidId);
                }
                match &item.album {
                    Some(album) => {
                        let album = self.local(album)?;
                        if album.is_empty() || !album.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(ProviderError::InvalidId);
                        }
                        Ok(format!("{track}:{album}"))
                    }
                    None => Ok(track.to_owned()),
                }
            })
            .collect::<Result<Vec<_>, ProviderError>>()?;
        let value = self.radio_post(
            &format!("/rotor/session/{}/tracks", session.id),
            serde_json::json!({"queue":queue}),
        )?;
        if value["unknownSession"].as_bool() == Some(true) {
            return Err(RadioError::UnknownSession);
        }
        radio_batch(&value)
    }
    fn playlist_snapshot(&self, id: &TrackId) -> Result<PlaylistSnapshot, ProviderError> {
        self.playlist_snapshot_unverified(id)
    }
    fn edit_playlist(
        &self,
        snapshot: &PlaylistSnapshot,
        edit: PlaylistEdit,
    ) -> Result<PlaylistSnapshot, PlaylistEditError> {
        self.edit_playlist_unverified(snapshot, edit)
    }
    fn info(&self) -> &ProviderInfo {
        &self.info
    }
    fn login(&self, token: Token) -> Result<Account, ProviderError> {
        if token.expose().trim().is_empty() {
            return Err(ProviderError::Auth);
        }
        self.set_token(token)?;
        self.account()
    }
    fn sign_in_start(
        &self,
        method: SignInMethod,
        _input: SignInInput,
    ) -> Result<SignInStep, ProviderError> {
        match method {
            SignInMethod::DeviceCode => self.device_code_start(),
            _ => Err(ProviderError::Unavailable),
        }
    }
    fn sign_in_continue(
        &self,
        flow: &SignInFlow,
        code: Option<Token>,
    ) -> Result<SignInStep, ProviderError> {
        match code {
            None => self.device_code_poll(flow),
            Some(_) => Err(ProviderError::Unavailable),
        }
    }
    fn logout(&self) {
        if let Ok(mut token) = self.token.write() {
            *token = None;
        }
        if let Ok(mut uid) = self.uid.lock() {
            *uid = None;
        }
        if let Ok(mut liked) = self.liked_ids.lock() {
            *liked = None;
        }
    }
    fn account(&self) -> Result<Account, ProviderError> {
        let v = self.api("/account/status", None)?;
        Ok(Account {
            id: ident(&v["account"]["uid"])?,
            name: ["login", "displayName"]
                .iter()
                .filter_map(|key| v["account"][key].as_str())
                .map(str::trim)
                .find(|name| !name.is_empty())
                .map(str::to_owned),
            subscription: v["plus"]["hasPlus"].as_bool().unwrap_or(false),
            capabilities: self.info.capabilities,
        })
    }
    fn search(&self, query: &str, page: u32) -> Result<Page<Track>, ProviderError> {
        let v = self.api(
            &format!(
                "/search?type=track&text={}&page={page}&nocorrect=true",
                encode(query)
            ),
            None,
        )?;
        if v["tracks"].is_null() {
            return Ok(Page {
                items: vec![],
                next: None,
            });
        }
        let items = tracks(&v["tracks"]["results"])?;
        let per = v["tracks"]["perPage"]
            .as_u64()
            .ok_or(ProviderError::ApiChanged)?;
        let total = v["tracks"]["total"]
            .as_u64()
            .ok_or(ProviderError::ApiChanged)?;
        Ok(Page {
            items,
            next: (per > 0 && (u64::from(page) + 1) * per < total).then(|| (page + 1).to_string()),
        })
    }
    fn search_artists(&self, query: &str, page: u32) -> Result<Page<Artist>, ProviderError> {
        self.search_section(query, page, "artist", "artists", parse_artist)
    }
    fn search_albums(&self, query: &str, page: u32) -> Result<Page<Album>, ProviderError> {
        self.search_section(query, page, "album", "albums", parse_album)
    }
    fn search_playlists(&self, query: &str, page: u32) -> Result<Page<Playlist>, ProviderError> {
        self.search_section(query, page, "playlist", "playlists", |value| {
            self.parse_playlist(value)
        })
    }
    // Request contract: https://github.com/MarshalX/yandex-music-api/blob/main/yandex_music/_client_async/playlists.py
    fn create_playlist(&self, title: &str) -> Result<Playlist, ProviderError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ProviderError::InvalidId);
        }
        let uid = self.uid()?;
        if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ProviderError::ApiChanged);
        }
        let value = self
            .api(
                &format!("/users/{uid}/playlists/create"),
                Some(vec![
                    ("title".into(), title.into()),
                    ("visibility".into(), "private".into()),
                ]),
            )
            .map_err(uncertain_write)?;
        let playlist = self
            .parse_playlist(&value)
            .map_err(|_| ProviderError::Uncertain)?;
        if playlist
            .id
            .local()
            .split_once('_')
            .is_none_or(|(owner, kind)| {
                owner != uid || kind.is_empty() || !kind.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(ProviderError::Uncertain);
        }
        Ok(playlist)
    }
    fn delete_playlist(&self, id: &TrackId) -> Result<(), ProviderError> {
        let local = self.local(id)?;
        let (owner, kind) = local.split_once('_').ok_or(ProviderError::InvalidId)?;
        if owner.is_empty()
            || kind.is_empty()
            || !owner.bytes().all(|b| b.is_ascii_digit())
            || !kind.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ProviderError::InvalidId);
        }
        let uid = self.uid()?;
        if owner != uid {
            return Err(ProviderError::InvalidId);
        }
        let result = self
            .api(
                &format!("/users/{uid}/playlists/{kind}/delete"),
                Some(Vec::new()),
            )
            .map_err(uncertain_write)?;
        if result.as_str() != Some("ok") {
            return Err(ProviderError::Uncertain);
        }
        Ok(())
    }
    fn track(&self, id: &TrackId) -> Result<Track, ProviderError> {
        let v = self.api(&format!("/tracks/{}", self.local(id)?), None)?;
        parse_track(
            v.as_array()
                .and_then(|a| a.first())
                .ok_or(ProviderError::NotFound)?,
        )
    }
    fn album(&self, id: &TrackId) -> Result<Album, ProviderError> {
        let v = self.api(&format!("/albums/{}/with-tracks", self.local(id)?), None)?;
        let mut all = vec![];
        for volume in v["volumes"].as_array().ok_or(ProviderError::ApiChanged)? {
            all.extend(tracks(volume)?);
        }
        Ok(Album {
            id: id.clone(),
            title: text(&v, "title")?.into(),
            artists: artists(&v["artists"])?,
            tracks: all,
            cover: v["coverUri"].as_str().map(str::to_owned),
            year: v["year"].as_u64().and_then(|y| u32::try_from(y).ok()),
        })
    }
    fn playlist(&self, id: &TrackId) -> Result<Playlist, ProviderError> {
        let (owner, kind) = self
            .local(id)?
            .split_once('_')
            .ok_or(ProviderError::InvalidId)?;
        self.parse_playlist(&self.api(&format!("/users/{owner}/playlists/{kind}"), None)?)
    }
    fn playlists(&self, cursor: Option<&str>) -> Result<Page<Playlist>, ProviderError> {
        if cursor.is_some() {
            return Err(ProviderError::InvalidId);
        }
        let uid = self.uid()?;
        let v = self.api(&format!("/users/{uid}/playlists/list"), None)?;
        Ok(Page {
            items: v
                .as_array()
                .ok_or(ProviderError::ApiChanged)?
                .iter()
                .map(|p| self.parse_playlist(p))
                .collect::<Result<_, _>>()?,
            next: None,
        })
    }
    fn likes(&self, cursor: Option<&str>) -> Result<Page<Track>, ProviderError> {
        let start = cursor
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| ProviderError::InvalidId)?;
        if start == 0 {
            let uid = self.uid()?;
            let v = self.api(&format!("/users/{uid}/likes/tracks"), None)?;
            let ids = v["library"]["tracks"]
                .as_array()
                .ok_or(ProviderError::ApiChanged)?
                .iter()
                .map(|t| ident(&t["id"]))
                .collect::<Result<Vec<_>, _>>()?;
            *self
                .liked_ids
                .lock()
                .map_err(|_| ProviderError::ApiChanged)? = Some(ids);
        }
        let ids = self
            .liked_ids
            .lock()
            .map_err(|_| ProviderError::ApiChanged)?;
        let ids = ids.as_ref().ok_or(ProviderError::ApiChanged)?;
        if start > ids.len() {
            return Err(ProviderError::InvalidId);
        }
        let end = (start + 100).min(ids.len());
        let items = if start == end {
            vec![]
        } else {
            tracks(&self.api(
                "/tracks",
                Some(vec![("track-ids".into(), ids[start..end].join(","))]),
            )?)?
        };
        Ok(Page {
            items,
            next: (end < ids.len()).then(|| end.to_string()),
        })
    }
    fn set_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError> {
        let uid = self.uid()?;
        let action = if liked { "add-multiple" } else { "remove" };
        let v = self.api(
            &format!("/users/{uid}/likes/tracks/{action}"),
            Some(vec![("track-ids".into(), self.local(id)?.into())]),
        )?;
        if v.get("revision").is_none() {
            return Err(ProviderError::ApiChanged);
        }
        Ok(())
    }
    fn set_disliked(&self, id: &TrackId, disliked: bool) -> Result<(), ProviderError> {
        let uid = self.uid()?;
        let action = if disliked { "add-multiple" } else { "remove" };
        self.api(
            &format!("/users/{uid}/dislikes/tracks/{action}"),
            Some(vec![("track-ids".into(), self.local(id)?.into())]),
        )?;
        Ok(())
    }
    fn liked_albums(&self) -> Result<Vec<Album>, ProviderError> {
        let uid = self.uid()?;
        let v = self.api(&format!("/users/{uid}/likes/albums?rich=true"), None)?;
        v.as_array()
            .ok_or(ProviderError::ApiChanged)?
            .iter()
            .map(parse_album)
            .collect()
    }
    fn liked_artists(&self) -> Result<Vec<Artist>, ProviderError> {
        let uid = self.uid()?;
        let v = self.api(
            &format!("/users/{uid}/likes/artists?with-timestamps=true"),
            None,
        )?;
        v.as_array()
            .ok_or(ProviderError::ApiChanged)?
            .iter()
            .map(parse_artist)
            .collect()
    }
    fn set_album_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError> {
        self.set_collection_liked("album", id, liked)
    }
    fn set_artist_liked(&self, id: &TrackId, liked: bool) -> Result<(), ProviderError> {
        self.set_collection_liked("artist", id, liked)
    }
    fn artist(&self, id: &TrackId) -> Result<ArtistPage, ProviderError> {
        let v = self.api(&format!("/artists/{}/brief-info", self.local(id)?), None)?;
        Ok(ArtistPage {
            artist: parse_artist(&v["artist"])?,
            tracks: if v["popularTracks"].is_array() {
                tracks(&v["popularTracks"])?
            } else {
                Vec::new()
            },
            albums: v["albums"]
                .as_array()
                .map(|a| a.iter().filter_map(|a| parse_album(a).ok()).collect())
                .unwrap_or_default(),
        })
    }
    fn resolve_stream(&self, id: &TrackId, quality: Quality) -> Result<Stream, ProviderError> {
        for attempt in 0..2 {
            let v = self.api_at(
                &format!("/tracks/{}/download-info", self.local(id)?),
                None,
                Priority::Playback,
            )?;
            let variant = select_variant(&v, quality)?;
            let url = text(variant, "downloadInfoUrl")?;
            if !url.starts_with("https://") {
                return Err(ProviderError::ApiChanged);
            }
            let response = match self.net.request(
                Priority::Playback,
                Request::get(SignedUrl::new(url.into())),
                None,
            ) {
                Err(HttpError::Status(410)) if attempt == 0 => continue,
                result => result.map_err(net_error)?,
            };
            let url = direct_link(&response.bytes)?;
            return Ok(Stream {
                bitrate_kbps: variant["bitrateInKbps"]
                    .as_u64()
                    .and_then(|b| u32::try_from(b).ok()),
                url: SignedUrl::new(url),
                codec: "mp3".into(),
                expires_at: SystemTime::now() + Duration::from_secs(50),
                encryption: Encryption::None,
            });
        }
        Err(ProviderError::Network)
    }
}
fn select_variant(v: &Value, quality: Quality) -> Result<&Value, ProviderError> {
    let variant = v
        .as_array()
        .ok_or(ProviderError::ApiChanged)?
        .iter()
        .filter(|v| {
            v["codec"] == "mp3"
                && v["preview"] != true
                && v["bitrateInKbps"].as_u64().is_some_and(|b| match quality {
                    Quality::High => b == 320,
                    Quality::Standard => (1..=192).contains(&b),
                })
        })
        .max_by_key(|v| v["bitrateInKbps"].as_u64())
        .ok_or(ProviderError::UnsupportedQuality)?;
    Ok(variant)
}
fn direct_link(xml: &[u8]) -> Result<String, ProviderError> {
    let s = std::str::from_utf8(xml).map_err(|_| ProviderError::ApiChanged)?;
    let doc = roxmltree::Document::parse(s).map_err(|_| ProviderError::ApiChanged)?;
    let field = |name| {
        doc.descendants()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .ok_or(ProviderError::ApiChanged)
    };
    let host = field("host")?;
    let path = field("path")?;
    let ts = field("ts")?;
    let salt = field("s")?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || !path.starts_with('/')
        || ts.is_empty()
        || !ts.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ProviderError::ApiChanged);
    }
    let sign = md5::compute(format!("XGRlBW9FXlekgbPrRHuSiA{}{salt}", &path[1..]));
    Ok(format!("https://{host}/get-mp3/{sign:x}/{ts}{path}"))
}
#[cfg(test)]
mod tests {
    use eagle_net::Client;
    use eagle_provider_sdk::Quality;
    use serde_json::{Value, json};

    #[test]
    fn scrub_recording_replaces_account_data_and_trims_likes() {
        let mut path = "/users/12345/likes/tracks".to_owned();
        let mut query = String::new();
        let mut body = json!({"result": {
            "uid": 12345,
            "login": "real.login",
            "email": "me@example.com",
            "displayName": "Real Name",
            "phones": ["+7 999 000-00-00"],
            "library": {
                "uid": 12345,
                "playlistUuid": "lk.9f1c-real",
                "tracks": [
                    {"id": 1, "timestamp": "2026-11-13T09:48:12+00:00"},
                    {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5}
                ]
            },
            "title": "Keep me"
        }});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(path, "/users/1/likes/tracks");
        assert_eq!(body["result"]["uid"], "1");
        assert_eq!(body["result"]["login"], "stub-login");
        assert_eq!(body["result"]["email"], "stub@example.invalid");
        assert_eq!(body["result"]["displayName"], "Stub User");
        assert_eq!(body["result"]["phones"], "+70000000000");
        assert_eq!(body["result"]["library"]["uid"], "1");
        assert_eq!(body["result"]["library"]["playlistUuid"], "stub-uuid");
        assert_eq!(
            body["result"]["library"]["tracks"][0]["timestamp"],
            "2000-01-01T00:00:00+00:00"
        );
        assert_eq!(
            body["result"]["library"]["tracks"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(body["result"]["title"], "Keep me");
    }

    #[test]
    fn scrub_recording_replaces_signed_download_link() {
        let mut path = "/tracks/128582477/download-info".to_owned();
        let mut query = String::new();
        let mut body = json!({"result": [{
            "bitrateInKbps": 192,
            "codec": "mp3",
            "direct": false,
            "preview": false,
            "downloadInfoUrl": "https://api.music.yandex.net/get-mp3/U2FsdGVkX1_LZLuOMnEq?sign=N2L8Zn-SFhgE&ts=1a1d5af7d83"
        }]});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(path, "/tracks/128582477/download-info");
        assert_eq!(
            body["result"][0]["downloadInfoUrl"],
            "https://api.music.yandex.net/get-mp3/stub?sign=stub&ts=0"
        );
        // The provider must still choose the variant from the scrubbed answer.
        let variant = super::select_variant(&body["result"], Quality::Standard).unwrap();
        assert_eq!(variant["bitrateInKbps"], 192);
    }

    #[test]
    fn scrub_recording_replaces_binary_download_with_stub_xml() {
        let mut path =
            "/get-mp3/U2FsdGVkX1_LZLuOMnEq_EZDKs2aBH_Eucy865Qi135Tfisxx6V77aRBjYyqQzh".to_owned();
        let mut query = "ts=1a1d5af7d83".to_owned();
        let mut body = Value::Null;
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(path, "/get-mp3/stub");
        assert_eq!(query, "ts=0");
        assert_eq!(body, Value::String(super::STUB_DOWNLOAD_XML.to_owned()));
        // The stub must still parse as a download answer.
        let link = super::direct_link(super::STUB_DOWNLOAD_XML.as_bytes()).unwrap();
        assert!(link.starts_with("https://stub.invalid/get-mp3/"), "{link}");
    }

    #[test]
    fn scrub_recording_neutralises_playlists_and_keeps_two() {
        let mut path = "/users/42/playlists/list".to_owned();
        let mut query = String::new();
        let playlist = json!({
            "title": "Мой плейлист",
            "description": "личное описание",
            "created": "2026-09-23T21:16:04+00:00",
            "modified": "2026-09-23T21:57:45+00:00",
            "playlistUuid": "f2a7c1d0-1111-2222-3333-444455556666",
            "ogImage": "avatars.yandex.net/get-music-content/9838169/16d29f89.a.28095380-2/%%",
            "uid": 42,
            "owner": {"uid": 42, "login": "real.login", "name": "Оскорбин Олег", "sex": "male"},
            "cover": {
                "type": "mosaic",
                "custom": false,
                "itemsUri": [
                    "avatars.yandex.net/get-music-content/2411511/c5152d03.a.4849007-4/%%",
                    "avatars.yandex.net/get-music-content/20305588/a5e64df5.a.36229847-1/%%"
                ]
            },
            "customWave": {
                "title": "ew?",
                "header": "Моя волна по плейлисту",
                "animationUrl": "https://music-custom-wave-media.s3.yandex.net/base.json",
                "backgroundImageUrl": "avatars.mds.yandex.net/get-music-misc/28052/bg/%%"
            },
            "trackCount": 12
        });
        let mut body = json!({"result": [playlist.clone(), playlist.clone(), playlist]});
        super::scrub_recording(&mut path, &mut query, &mut body);
        let items = body["result"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let first = &items[0];
        assert_eq!(first["title"], "Stub playlist");
        assert_eq!(first["description"], "stub");
        assert_eq!(first["created"], "2000-01-01T00:00:00+00:00");
        assert_eq!(first["modified"], "2000-01-01T00:00:00+00:00");
        assert_eq!(first["playlistUuid"], "stub-uuid");
        assert_eq!(first["ogImage"], "avatars.yandex.net/stub/%%");
        assert_eq!(first["uid"], "1");
        assert_eq!(first["owner"]["name"], "Stub User");
        assert_eq!(first["owner"]["login"], "stub-login");
        assert_eq!(first["owner"]["uid"], "1");
        assert_eq!(first["cover"]["type"], "mosaic");
        assert_eq!(
            first["cover"]["itemsUri"],
            json!(["avatars.yandex.net/stub/%%"])
        );
        assert_eq!(first["customWave"]["title"], "stub");
        assert_eq!(first["customWave"]["header"], "stub");
        assert_eq!(first["customWave"]["animationUrl"], "stub");
        assert_eq!(first["customWave"]["backgroundImageUrl"], "stub");
        assert_eq!(first["trackCount"], 12);
    }

    #[test]
    fn scrub_recording_replaces_avatar_urls() {
        let mut path = "/users/42/likes/tracks".to_owned();
        let mut query = String::new();
        let mut body = json!({"result": {
            "avatarUri": "https://avatars.yandex.net/get-music-content/1/abc/%%",
            "avatarsUrl": "avatars.yandex.net/get-music-content/9/x"
        }});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(body["result"]["avatarUri"], "avatars.yandex.net/stub/%%");
        assert_eq!(body["result"]["avatarsUrl"], "avatars.yandex.net/stub/%%");
    }

    #[test]
    fn scrub_recording_replaces_profile_fields_of_account_status() {
        let mut path = "/account/status".to_owned();
        let mut query = String::new();
        let mut body = json!({
            "account": {
                "uid": 12345,
                "login": "real.login",
                "displayName": "Real Name",
                "secondName": "Настоящая",
                "birthday": "2006-10-23",
                "region": 225
            },
            "defaultEmail": "real.login@yandex.ru",
            "userhash": "b0f3c1a9d2e4",
            "subscription": {"autoRenewable": [{"orderId": 42, "productId": "plus"}]}
        });
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(body["account"]["secondName"], "User");
        assert_eq!(body["account"]["birthday"], "2000-01-01");
        assert_eq!(body["defaultEmail"], "stub@example.invalid");
        assert_eq!(body["userhash"], "stub-hash");
        assert_eq!(
            body["subscription"]["autoRenewable"][0]["orderId"],
            "stub-order"
        );
        assert_eq!(
            body["subscription"]["autoRenewable"][0]["productId"],
            "plus"
        );
        assert_eq!(body["account"]["region"], 225);
    }

    #[test]
    fn scrub_recording_keeps_five_search_results_per_kind() {
        let mut path = "/search".to_owned();
        let mut query = "nocorrect=true&page=0&text=Queen&type=track".to_owned();
        let mut body = json!({"result": {
            "tracks": {"results": [1, 2, 3, 4, 5, 6, 7], "total": 7},
            "artists": {"results": [1, 2, 3, 4, 5, 6]},
            "albums": {"results": [1]},
            "playlists": {"results": [1, 2, 3, 4, 5, 6, 7, 8]},
            "text": "Queen"
        }});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(path, "/search");
        assert_eq!(query, "nocorrect=true&page=0&text=Queen&type=track");
        assert_eq!(
            body["result"]["tracks"]["results"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            body["result"]["artists"]["results"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(
            body["result"]["albums"]["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            body["result"]["playlists"]["results"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(body["result"]["text"], "Queen");
    }

    #[test]
    fn scrub_recording_keeps_three_tracks_from_batch_lookup() {
        let mut path = "/tracks".to_owned();
        let mut query = String::new();
        let mut body = json!({"result": [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}]});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(body["result"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn scrub_recording_leaves_query_of_other_requests_alone() {
        let mut path = "/search".to_owned();
        let mut query = "text=Queen&type=track".to_owned();
        let mut body = json!({"result": {}});
        super::scrub_recording(&mut path, &mut query, &mut body);
        assert_eq!(query, "text=Queen&type=track");
    }

    #[test]
    fn scrub_recording_is_idempotent() {
        let cases = vec![
            (
                "/users/42/playlists/list".to_owned(),
                String::new(),
                json!({"result": [
                    {"title": "a", "owner": {"uid": 42, "name": "x"},
                     "customWave": {"title": "t"},
                     "cover": {"itemsUri": ["avatars.yandex.net/get-music-content/1/x/%%"]}},
                    {}, {}
                ]}),
            ),
            (
                "/users/42/likes/tracks".to_owned(),
                String::new(),
                json!({"result": {"library": {"uid": 42, "tracks": [
                    {"timestamp": "2026-11-13T09:48:12+00:00"}, {}, {}, {}
                ]}}}),
            ),
            (
                "/search".to_owned(),
                "text=Queen".to_owned(),
                json!({"result": {"tracks": {"results": [1, 2, 3, 4, 5, 6]}}}),
            ),
            (
                "/tracks".to_owned(),
                String::new(),
                json!({"result": [1, 2, 3, 4]}),
            ),
            (
                "/tracks/1/download-info".to_owned(),
                String::new(),
                json!({"result": [{"downloadInfoUrl":
                    "https://api.music.yandex.net/get-mp3/U2FsdGVkX1abc?sign=q&ts=1a"}]}),
            ),
            (
                "/get-mp3/U2FsdGVkX1abc".to_owned(),
                "ts=1a".to_owned(),
                Value::Null,
            ),
        ];
        for (mut path, mut query, mut body) in cases {
            super::scrub_recording(&mut path, &mut query, &mut body);
            let once = (path.clone(), query.clone(), body.clone());
            super::scrub_recording(&mut path, &mut query, &mut body);
            assert_eq!((path, query, body), once);
        }
    }

    #[test]
    fn info_passes_conformance_checks() {
        let provider = Yandex::new(Arc::new(Client::new().unwrap()));
        let violations = eagle_provider_sdk::testkit::conformance::check_info(provider.info());
        assert!(violations.is_empty(), "{violations:?}");
    }
    fn mock_api(responses: Vec<&'static str>) -> (Yandex, std::thread::JoinHandle<Vec<String>>) {
        mock_api_status(responses.into_iter().map(|body| (200, body)).collect())
    }
    /// Like `mock_api`, with the HTTP status of every answer; OAuth base points at the mock too.
    fn mock_api_status(
        responses: Vec<(u16, &'static str)>,
    ) -> (Yandex, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0u8; 1];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let header = String::from_utf8(bytes.clone()).unwrap();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                bytes.extend(body);
                requests.push(String::from_utf8(bytes).unwrap());
                write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
            requests
        });
        let mut provider = Yandex::new(Arc::new(Client::new().unwrap()));
        provider.api_base = Some(format!("http://{address}"));
        provider.oauth_base = Some(format!("http://{address}"));
        *provider.token.write().unwrap() = Some(Token::new("fixture-token".into()));
        *provider.uid.lock().unwrap() = Some("42".into());
        (provider, worker)
    }
    #[test]
    fn device_code_flow_shows_a_code_waits_and_returns_the_token() {
        let (provider, server) = mock_api_status(vec![
            (
                200,
                r#"{"device_code":"dev-1","user_code":"abc123","verification_url":"https://ya.ru/device","interval":7,"expires_in":240}"#,
            ),
            (400, r#"{"error":"authorization_pending"}"#),
            (200, r#"{"access_token":"tok-1","token_type":"bearer"}"#),
        ]);
        let SignInStep::ShowCode {
            code,
            url,
            interval,
            expires_in,
            flow,
        } = provider
            .sign_in_start(SignInMethod::DeviceCode, SignInInput::None)
            .unwrap()
        else {
            panic!("expected ShowCode");
        };
        assert_eq!(code, "abc123");
        assert_eq!(url, "https://ya.ru/device");
        assert_eq!(interval, Duration::from_secs(7));
        assert_eq!(expires_in, Duration::from_secs(240));
        assert!(matches!(
            provider.sign_in_continue(&flow, None),
            Ok(SignInStep::Pending)
        ));
        let SignInStep::Done(token) = provider.sign_in_continue(&flow, None).unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(token.expose(), "tok-1");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /device/code "));
        assert!(requests[0].contains("client_id=23cabbbdc6cd418abb4b39c32c41195d"));
        assert!(requests[1].starts_with("POST /token "));
        assert!(requests[1].contains("grant_type=device_code"));
        assert!(requests[1].contains("code=dev-1"));
        assert!(requests[1].contains("client_secret="));
    }
    #[test]
    fn device_code_poll_maps_oauth_errors() {
        let (provider, server) = mock_api_status(vec![
            (400, r#"{"error":"expired_token"}"#),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"Code has expired"}"#,
            ),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"Wrong code"}"#,
            ),
            (400, r#"{"error":"access_denied"}"#),
            (400, r#"not json"#),
            (200, r#"{"token_type":"bearer"}"#),
            (400, r#"{"error":"slow_down"}"#),
            (429, r#"Too Many Requests"#),
        ]);
        let flow = SignInFlow(Token::new("dev-1".into()));
        assert!(matches!(
            provider.sign_in_continue(&flow, None),
            Ok(SignInStep::Expired)
        ));
        assert!(matches!(
            provider.sign_in_continue(&flow, None),
            Ok(SignInStep::Expired)
        ));
        for _ in 0..2 {
            assert!(matches!(
                provider.sign_in_continue(&flow, None),
                Err(ProviderError::Auth)
            ));
        }
        for _ in 0..2 {
            assert!(matches!(
                provider.sign_in_continue(&flow, None),
                Err(ProviderError::ApiChanged)
            ));
        }
        for _ in 0..2 {
            assert!(matches!(
                provider.sign_in_continue(&flow, None),
                Err(ProviderError::RateLimited)
            ));
        }
        server.join().unwrap();
    }
    #[test]
    fn device_code_start_falls_back_to_defaults_and_rejects_bad_answers() {
        let (provider, server) = mock_api_status(vec![
            (200, r#"{"device_code":"d","user_code":"u"}"#),
            (400, r#"{"error":"invalid_client"}"#),
            (200, r#"{"user_code":"u"}"#),
        ]);
        let Ok(SignInStep::ShowCode {
            url,
            interval,
            expires_in,
            ..
        }) = provider.sign_in_start(SignInMethod::DeviceCode, SignInInput::None)
        else {
            panic!("expected ShowCode");
        };
        assert_eq!(url, "https://ya.ru/device");
        assert_eq!(interval, Duration::from_secs(5));
        assert_eq!(expires_in, Duration::from_secs(300));
        for _ in 0..2 {
            assert!(matches!(
                provider.sign_in_start(SignInMethod::DeviceCode, SignInInput::None),
                Err(ProviderError::ApiChanged)
            ));
        }
        server.join().unwrap();
    }
    #[test]
    fn unsupported_sign_in_methods_do_not_touch_the_network() {
        let provider = Yandex::new(Arc::new(Client::new().unwrap()));
        for method in [
            SignInMethod::Browser,
            SignInMethod::Password { login_hint: "" },
        ] {
            assert!(matches!(
                provider.sign_in_start(method, SignInInput::None),
                Err(ProviderError::Unavailable)
            ));
        }
        let flow = SignInFlow(Token::new("d".into()));
        assert!(matches!(
            provider.sign_in_continue(&flow, Some(Token::new("1".into()))),
            Err(ProviderError::Unavailable)
        ));
    }
    #[test]
    fn account_name_prefers_login_over_display_name() {
        let (provider, server) = mock_api(vec![
            r#"{"result":{"account":{"uid":42,"login":"anna.k","displayName":"Anna"},"plus":{"hasPlus":true}}}"#,
            r#"{"result":{"account":{"uid":42,"login":" ","displayName":"Anna"}}}"#,
            r#"{"result":{"account":{"uid":42}}}"#,
        ]);
        assert_eq!(provider.account().unwrap().name.as_deref(), Some("anna.k"));
        assert_eq!(provider.account().unwrap().name.as_deref(), Some("Anna"));
        assert_eq!(provider.account().unwrap().name, None);
        server.join().unwrap();
    }
    #[test]
    fn category_search_requests_and_pagination() {
        let (provider, server) = mock_api(vec![
            r#"{"result":{"artists":{"results":[{"id":1,"name":"Artist","cover":{"uri":"cover/%%"}}],"perPage":1,"total":2}}}"#,
            r#"{"result":{"albums":{"results":[{"id":2,"title":"Album","artists":[]}],"perPage":20,"total":1}}}"#,
            r#"{"result":{"playlists":{"results":[{"owner":{"uid":42},"kind":3,"title":"Playlist","cover":{"uri":"playlist/%%"}}],"perPage":20,"total":1}}}"#,
            r#"{"result":{"artists":null}}"#,
            r#"{"result":{"albums":{"results":{},"perPage":20,"total":1}}}"#,
        ]);
        let artists = provider.search_artists("a b", 0).unwrap();
        assert_eq!(artists.next.as_deref(), Some("1"));
        assert_eq!(artists.items[0].cover.as_deref(), Some("cover/%%"));
        assert_eq!(
            provider.search_albums("q", 0).unwrap().items[0]
                .id
                .to_string(),
            "yandex:2"
        );
        let playlists = provider.search_playlists("q", 0).unwrap();
        assert_eq!(playlists.items[0].id.to_string(), "yandex:42_3");
        assert_eq!(playlists.items[0].cover.as_deref(), Some("playlist/%%"));
        assert!(provider.search_artists("q", 0).unwrap().items.is_empty());
        assert!(matches!(
            provider.search_albums("q", 0),
            Err(ProviderError::ApiChanged)
        ));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /search?type=artist&text=a%20b&page=0&nocorrect=true ")
        );
        assert!(requests[1].starts_with("GET /search?type=album&"));
        assert!(requests[2].starts_with("GET /search?type=playlist&"));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("OAuth fixture-token"))
        );
    }
    #[test]
    fn playlist_create_delete_validate_identity_and_responses() {
        let (provider, server) = mock_api(vec![
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"New"}}"#,
            r#"{"result":"ok"}"#,
            r#"{"result":false}"#,
            r#"{"result":{"owner":{"uid":42},"kind":"oops","title":"Bad"}}"#,
        ]);
        assert!(provider.create_playlist("  ").is_err());
        let playlist = provider.create_playlist(" New ").unwrap();
        assert_eq!(playlist.id.to_string(), "yandex:42_3");
        assert!(
            provider
                .delete_playlist(&"yandex:99_3".parse().unwrap())
                .is_err()
        );
        assert!(
            provider
                .delete_playlist(&"vk:42_3".parse().unwrap())
                .is_err()
        );
        assert!(
            provider
                .delete_playlist(&"yandex:42_3_extra".parse().unwrap())
                .is_err()
        );
        provider.delete_playlist(&playlist.id).unwrap();
        assert!(matches!(
            provider.delete_playlist(&playlist.id),
            Err(ProviderError::Uncertain)
        ));
        assert!(matches!(
            provider.create_playlist("Bad"),
            Err(ProviderError::Uncertain)
        ));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /users/42/playlists/create "));
        assert!(requests[0].contains("title=New"));
        assert!(requests[0].contains("visibility=private"));
        assert!(requests[1].starts_with("POST /users/42/playlists/3/delete "));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("OAuth fixture-token"))
        );
    }
    #[test]
    fn radio_start_next_and_unknown_session_use_json_without_feedback() {
        let start = r#"{"result":{"radioSessionId":"fixture-session","batchId":"batch-1","sequence":[{"track":{"id":"7","title":"A","artists":[]}}]}}"#;
        let next = r#"{"result":{"batchId":"batch-2","unknownSession":false,"sequence":[{"track":{"id":"7","title":"A","artists":[]}}]}}"#;
        let unknown = r#"{"result":{"unknownSession":true}}"#;
        let (provider, server) = mock_api(vec![start, next, unknown]);
        let session = provider.radio_start(&RadioTuning::default()).unwrap();
        let queue = [RadioQueueItem {
            track: "yandex:7".parse().unwrap(),
            album: Some("yandex:8".parse().unwrap()),
        }];
        assert_eq!(
            provider.radio_next(&session, &queue).unwrap().tracks.len(),
            1
        );
        assert!(matches!(
            provider.radio_next(&session, &[]),
            Err(RadioError::UnknownSession)
        ));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /rotor/session/new "));
        assert!(requests[0].contains("application/json"));
        assert!(requests[0].contains(r#""seeds":["user:onyourwave"]"#));
        assert!(!requests[0].contains("setting"));
        assert!(requests[1].starts_with("POST /rotor/session/fixture-session/tracks "));
        assert!(requests[1].contains("7:8"));
        assert!(!requests.iter().any(|r| r.contains("feedback")));
        let invalid = RadioSession {
            id: "bad/path?x".into(),
            batch: session.batch,
        };
        assert!(matches!(
            provider.radio_next(&invalid, &[]),
            Err(RadioError::Provider(ProviderError::InvalidId))
        ));
    }
    #[test]
    fn radio_start_sends_mood_and_diversity_settings_and_the_seed_track() {
        let start = r#"{"result":{"radioSessionId":"fixture-session","batchId":"batch-1","sequence":[{"track":{"id":"7","title":"A","artists":[]}}]}}"#;
        let (provider, server) = mock_api(vec![start, start]);
        provider
            .radio_start(&RadioTuning {
                mood: Some((0.0, 0.9)),
                novelty: 0.9,
                seed: None,
            })
            .unwrap();
        provider
            .radio_start(&RadioTuning {
                mood: None,
                novelty: 0.1,
                seed: Some("yandex:7".parse().unwrap()),
            })
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].contains(
            r#""seeds":["user:onyourwave","settingMoodEnergy:active","settingDiversity:discover"]"#
        ));
        assert!(
            requests[1].contains(r#""seeds":["track:7","settingDiversity:favorite"]"#),
            "{}",
            requests[1]
        );
        assert!(requests.iter().all(|r| r.contains(r#""incognito":true"#)));
    }
    #[test]
    fn radio_seeds_map_all_mood_directions_and_reject_foreign_seeds() {
        let mood = |x: f32, y: f32| {
            radio_seeds(&RadioTuning {
                mood: Some((x, y)),
                ..RadioTuning::default()
            })
            .unwrap()
        };
        assert_eq!(mood(0.0, 0.0), ["user:onyourwave"]);
        assert_eq!(mood(0.05, 0.0), ["user:onyourwave"]);
        let expected = [
            ((1.0, 0.0), "fun"),
            ((0.7, 0.7), "fun"),
            ((0.0, 1.0), "active"),
            ((-0.7, 0.7), "active"),
            ((-1.0, 0.0), "sad"),
            ((-0.7, -0.7), "sad"),
            ((0.0, -1.0), "calm"),
            ((0.7, -0.7), "calm"),
        ];
        for ((x, y), setting) in expected {
            assert_eq!(
                mood(x, y)[1],
                format!("settingMoodEnergy:{setting}"),
                "{x},{y}"
            );
        }
        for seed in ["other:7", "yandex:abc", "yandex:7_8"] {
            let tuning = RadioTuning {
                seed: Some(seed.parse().unwrap()),
                ..RadioTuning::default()
            };
            assert!(matches!(
                radio_seeds(&tuning),
                Err(ProviderError::InvalidId)
            ));
        }
    }
    #[test]
    fn playlist_entry_album_identity_overrides_track_album() {
        let value: Value = serde_json::from_str(r#"{"owner":{"uid":42},"kind":3,"title":"Fixture","tracks":[{"albumId":"8","track":{"id":"7","title":"A","artists":[],"albums":[{"id":"99","title":"Other"},{"id":8,"title":"Matching"}]}},{"albumId":"10","track":{"id":"7","title":"A","artists":[],"albums":[{"id":"99","title":"Other"}]}},{"track":{"id":"7","title":"A","artists":[],"albums":[{"id":"99","title":"Other"}]}}]}"#).unwrap();
        let provider = Yandex::new(Arc::new(Client::new().unwrap()));
        let playlist = provider.parse_playlist(&value).unwrap();
        assert_eq!(playlist.tracks[0].album.as_ref().unwrap().local(), "8");
        assert_eq!(playlist.tracks[0].album_title.as_deref(), Some("Matching"));
        assert_eq!(playlist.tracks[1].album.as_ref().unwrap().local(), "10");
        assert!(playlist.tracks[1].album_title.is_none());
        assert!(playlist.tracks[2].album.is_none());
        assert!(playlist.tracks[2].album_title.is_none());
    }
    #[test]
    fn radio_sequence_fixture_preserves_duplicate_occurrences() {
        let response: Value = serde_json::from_str(r#"{"result":{"radioSessionId":"fixture-session","batchId":"fixture-batch","terminated":false,"sequence":[{"type":"track","liked":false,"track":{"id":"7","title":"A","artists":[]}},{"type":"track","liked":false,"track":{"id":"7","title":"A","artists":[]}}]}}"#).unwrap();
        let result = &response["result"];
        assert!(result["radioSessionId"].is_string());
        assert!(result["batchId"].is_string());
        let parsed = tracks(&result["sequence"]).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, parsed[1].id);
    }
    #[test]
    fn playlist_snapshot_rejects_missing_or_partial_tracks() {
        let (provider, server) = mock_api(vec![
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"trackCount":2,"tracks":[]}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"trackCount":0}}"#,
        ]);
        let id = "yandex:42_3".parse().unwrap();
        assert!(matches!(
            provider.playlist_snapshot(&id),
            Err(ProviderError::ApiChanged)
        ));
        assert!(matches!(
            provider.playlist_snapshot(&id),
            Err(ProviderError::ApiChanged)
        ));
        assert!(
            provider
                .playlist_snapshot(&id)
                .unwrap()
                .playlist
                .tracks
                .is_empty()
        );
        server.join().unwrap();
    }
    #[test]
    fn playlist_edit_reports_uncertain_when_post_read_differs() {
        let before = r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"tracks":[]}}"#;
        let after = r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[]}}"#;
        let (provider, server) = mock_api(vec![before, before, after, after]);
        let snapshot = provider
            .playlist_snapshot(&"yandex:42_3".parse().unwrap())
            .unwrap();
        let edit = PlaylistEdit::Insert {
            at: 0,
            tracks: vec![PlaylistTrack {
                track_id: "yandex:7".parse().unwrap(),
                album_id: "yandex:8".parse().unwrap(),
            }],
        };
        assert!(matches!(
            provider.edit_playlist(&snapshot, edit),
            Err(PlaylistEditError::Uncertain)
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests.iter().filter(|r| r.starts_with("POST ")).count(),
            1
        );
    }
    #[test]
    fn playlist_move_sends_delete_and_insert_for_exact_occurrence() {
        use crate::contracts::PlaylistEdit;
        let before = r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"tracks":[{"albumId":"8","track":{"id":"7","title":"A","artists":[],"albums":[{"id":"8"}]}},{"albumId":"10","track":{"id":"9","title":"B","artists":[],"albums":[{"id":"10"}]}}]}}"#;
        let after = r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[{"albumId":"10","track":{"id":"9","title":"B","artists":[],"albums":[{"id":"10"}]}},{"albumId":"8","track":{"id":"7","title":"A","artists":[],"albums":[{"id":"8"}]}}]}}"#;
        for (from, to) in [(0, 1), (1, 0)] {
            let (provider, server) = mock_api(vec![before, before, after, after]);
            let snapshot = provider
                .playlist_snapshot(&"yandex:42_3".parse().unwrap())
                .unwrap();
            let moved = provider
                .edit_playlist(&snapshot, PlaylistEdit::Move { from, to })
                .unwrap();
            assert_eq!(moved.playlist.tracks[0].id.local(), "9");
            let requests = server.join().unwrap();
            let request = &requests[2];
            let encoded = request.split("\r\n\r\n").nth(1).unwrap();
            let diff = encoded
                .split('&')
                .find_map(|part| part.strip_prefix("diff="))
                .unwrap();
            let bytes = diff.as_bytes();
            let mut decoded = Vec::new();
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'%' {
                    decoded.push(u8::from_str_radix(&diff[index + 1..index + 3], 16).unwrap());
                    index += 3;
                } else {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            let diff: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
            assert_eq!(
                diff[0],
                serde_json::json!({"op":"delete", "from":from, "to":from + 1})
            );
            assert_eq!(
                diff[1],
                serde_json::json!({"op":"insert", "at":to, "tracks":[{"id":if from == 0 { "7" } else { "9" }, "albumId":if from == 0 { "8" } else { "10" }}]})
            );
        }
    }
    #[test]
    fn playlist_snapshot_preserves_duplicate_track_order() {
        let (provider, server) = mock_api(vec![
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[{"id":"7","track":{"id":"7","title":"A","artists":[]}},{"id":"9","track":{"id":"9","title":"B","artists":[]}},{"id":"7","track":{"id":"7","title":"A","artists":[]}}]}}"#,
        ]);
        let snapshot = provider
            .playlist_snapshot_unverified(&"yandex:42_3".parse().unwrap())
            .unwrap();
        let ids: Vec<_> = snapshot
            .playlist
            .tracks
            .iter()
            .map(|track| track.id.local())
            .collect();
        assert_eq!(ids, ["7", "9", "7"]);
        assert_eq!(snapshot.revision, 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /users/42/playlists/3 "));
    }
    #[test]
    fn experimental_playlist_edit_checks_revision_and_sends_positional_diff() {
        use crate::contracts::{PlaylistEdit, PlaylistEditError, PlaylistTrack};
        let (provider, server) = mock_api(vec![
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"tracks":[]}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[]}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"tracks":[]}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[]}}"#,
            r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":2,"tracks":[{"albumId":"8","track":{"id":"7","title":"A","artists":[]}}]}}"#,
        ]);
        let snapshot = provider
            .playlist_snapshot_unverified(&"yandex:42_3".parse().unwrap())
            .unwrap();
        let insert = || PlaylistEdit::Insert {
            at: 0,
            tracks: vec![PlaylistTrack {
                track_id: "yandex:7".parse().unwrap(),
                album_id: "yandex:8".parse().unwrap(),
            }],
        };
        assert!(matches!(
            provider.edit_playlist_unverified(&snapshot, insert()),
            Err(PlaylistEditError::Conflict)
        ));
        let updated = provider
            .edit_playlist_unverified(&snapshot, insert())
            .unwrap();
        assert_eq!(updated.revision, 2);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[3].starts_with("POST /users/42/playlists/3/change "));
        assert!(requests[3].contains("revision=1"));
        assert!(requests[3].contains("albumId"));
    }
    #[test]
    fn experimental_playlist_edit_treats_bad_post_response_as_uncertain() {
        use crate::contracts::{PlaylistEdit, PlaylistEditError, PlaylistTrack};
        let snapshot_json = r#"{"result":{"owner":{"uid":42},"kind":3,"title":"Fixture","revision":1,"tracks":[]}}"#;
        let (provider, server) =
            mock_api(vec![snapshot_json, snapshot_json, r#"{"result":false}"#]);
        let snapshot = provider
            .playlist_snapshot_unverified(&"yandex:42_3".parse().unwrap())
            .unwrap();
        let edit = PlaylistEdit::Insert {
            at: 0,
            tracks: vec![PlaylistTrack {
                track_id: "yandex:7".parse().unwrap(),
                album_id: "yandex:8".parse().unwrap(),
            }],
        };
        assert!(matches!(
            provider.edit_playlist_unverified(&snapshot, edit),
            Err(PlaylistEditError::Uncertain)
        ));
        assert_eq!(server.join().unwrap().len(), 3);
    }
    #[test]
    fn playlist_lost_post_response_is_uncertain_without_retry() {
        use std::{io::Read, net::TcpListener, time::Instant};
        for delete in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut requests = 0;
                while Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(1)))
                                .unwrap();
                            let mut buf = [0; 4096];
                            let _ = stream.read(&mut buf);
                            requests += 1;
                            // Drop the connection without a response after receiving the POST.
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("listener: {e}"),
                    }
                }
                requests
            });
            let mut provider = Yandex::new(Arc::new(Client::new().unwrap()));
            provider.api_base = Some(format!("http://{address}"));
            *provider.token.write().unwrap() = Some(Token::new("fixture-token".into()));
            *provider.uid.lock().unwrap() = Some("42".into());
            let result = if delete {
                provider.delete_playlist(&"yandex:42_3".parse().unwrap())
            } else {
                provider.create_playlist("fixture").map(|_| ())
            };
            assert!(matches!(result, Err(ProviderError::Uncertain)));
            assert_eq!(server.join().unwrap(), 1);
        }
    }
    #[test]
    fn playlist_auth_failure_before_post_is_not_uncertain() {
        let provider = Yandex::new(Arc::new(Client::new().unwrap()));
        *provider.uid.lock().unwrap() = Some("42".into());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = provider.token.write().unwrap();
            panic!("poison token lock");
        }));
        assert!(matches!(
            provider.create_playlist("fixture"),
            Err(ProviderError::Auth)
        ));
    }
    #[test]
    fn playlist_cached_before_cover_field_still_loads() {
        let playlist: Playlist =
            serde_json::from_str(r#"{"id":"yandex:42_1","title":"test","tracks":[]}"#).unwrap();
        assert!(playlist.cover.is_none());
    }
    #[test]
    fn uid_caches_only_success_and_token_change_invalidates_it() {
        let provider = Yandex::new(Arc::new(Client::new().unwrap()));
        assert!(provider.cached_uid(|| Err(ProviderError::Network)).is_err());
        assert_eq!(provider.cached_uid(|| Ok("first".into())).unwrap(), "first");
        assert_eq!(
            provider
                .cached_uid(|| panic!("cached uid must be reused"))
                .unwrap(),
            "first"
        );
        *provider.liked_ids.lock().unwrap() = Some(vec!["1".into()]);
        provider.set_token(Token::new("new".into())).unwrap();
        assert!(provider.liked_ids.lock().unwrap().is_none());
        assert_eq!(
            provider.cached_uid(|| Ok("second".into())).unwrap(),
            "second"
        );
    }
    #[test]
    fn liked_albums_and_artists_unwrap_rich_items() {
        let album = parse_album(&serde_json::json!({
            "timestamp": "2026-01-01",
            "album": {"id": 7, "title": "Аналог", "year": 2022, "coverUri": "avatars.yandex.net/x/%%",
                      "artists": [{"id": 3, "name": "Ольга Корт"}]}
        }))
        .unwrap();
        assert_eq!(album.id.to_string(), "yandex:7");
        assert_eq!(album.year, Some(2022));
        assert_eq!(album.artists[0].name, "Ольга Корт");
        assert_eq!(album.cover.as_deref(), Some("avatars.yandex.net/x/%%"));
        let artist = parse_artist(&serde_json::json!({
            "artist": {"id": "3", "name": "Ольга Корт", "cover": {"uri": "avatars.yandex.net/a/%%"}}
        }))
        .unwrap();
        assert_eq!(artist.id.to_string(), "yandex:3");
        assert_eq!(artist.cover.as_deref(), Some("avatars.yandex.net/a/%%"));
        let bare = parse_artist(&serde_json::json!({"id": 4, "name": "Сигнал"})).unwrap();
        assert!(bare.cover.is_none());
    }
    use super::*;
    #[test]
    fn quality_selection_never_chooses_a_preview() {
        let v: Value =
            serde_json::from_str(include_str!("../tests/fixtures/download-info.json")).unwrap();
        assert_eq!(
            select_variant(&v, Quality::High).unwrap()["downloadInfoUrl"],
            "https://cdn.example.org/320"
        );
        assert_eq!(
            select_variant(&v, Quality::Standard).unwrap()["bitrateInKbps"],
            128
        );
        assert!(matches!(
            select_variant(&serde_json::json!([]), Quality::High),
            Err(ProviderError::UnsupportedQuality)
        ));
    }
    #[test]
    fn unavailable_quality_never_falls_back_or_selects_invalid_bitrate() {
        let low = serde_json::json!([
            {"codec":"mp3","bitrateInKbps":128,"preview":false},
            {"codec":"mp3","bitrateInKbps":320,"preview":true},
            {"codec":"aac","bitrateInKbps":320,"preview":false}
        ]);
        assert!(matches!(
            select_variant(&low, Quality::High),
            Err(ProviderError::UnsupportedQuality)
        ));
        assert_eq!(
            select_variant(&low, Quality::Standard).unwrap()["bitrateInKbps"],
            128
        );
        assert!(matches!(
            select_variant(
                &serde_json::json!([{ "codec":"mp3", "bitrateInKbps":0 }]),
                Quality::Standard
            ),
            Err(ProviderError::UnsupportedQuality)
        ));
        assert!(matches!(
            select_variant(&serde_json::json!({}), Quality::High),
            Err(ProviderError::ApiChanged)
        ));
    }
    #[test]
    fn fixture_and_schema_drift() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/track.json")).unwrap();
        let t = parse_track(&v).unwrap();
        assert_eq!(t.id.to_string(), "yandex:123");
        assert_eq!(t.availability, Availability::Available);
        assert!(parse_track(&serde_json::json!({"id":123})).is_err());
    }
    #[test]
    fn manifest_preserves_hexadecimal_timestamp() {
        let xml = include_str!("../tests/fixtures/download.xml")
            .replace("<ts>123</ts>", "<ts>6ae1b3f0</ts>");
        let link = direct_link(xml.as_bytes()).unwrap();
        assert!(link.ends_with("/6ae1b3f0/music.mp3"));
        for ts in ["", "not-a-timestamp", "123/other", "123?token=x"] {
            let invalid = xml.replace("6ae1b3f0", ts);
            assert!(direct_link(invalid.as_bytes()).is_err());
        }
    }
    #[test]
    fn manifest_has_signed_link() {
        let link = direct_link(include_bytes!("../tests/fixtures/download.xml")).unwrap();
        assert!(link.starts_with("https://cdn.example.org/get-mp3/"));
        assert!(link.ends_with("/123/music.mp3"));
        assert!(direct_link(b"<broken").is_err());
    }
}
