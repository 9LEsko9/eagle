// SPDX-License-Identifier: GPL-3.0-or-later
//! Blocking Yandex Music adapter. Secrets never enter SQLite or error text.
use eagle_core::*;
use eagle_net::{Client, Priority, Request};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime},
};
const API: &str = "https://api.music.yandex.net";
pub struct Yandex {
    net: Arc<Client>,
    #[cfg(test)]
    api_base: Option<String>,
    token: RwLock<Option<Token>>,
    liked_ids: Mutex<Option<Vec<String>>>,
    /// Account uid, cached so likes and lists cost one request instead of two.
    uid: Mutex<Option<String>>,
}
fn keyring() -> Result<keyring::Entry, ProviderError> {
    keyring::Entry::new("eagle", "yandex").map_err(|_| ProviderError::Keyring)
}
pub fn prompt_token() -> Result<Token, std::io::Error> {
    rpassword::prompt_password("Токен Яндекс Музыки: ").map(Token::new)
}
fn net_error(e: eagle_net::Error) -> ProviderError {
    match e {
        eagle_net::Error::Status(401) => ProviderError::Auth,
        eagle_net::Error::Status(403 | 451) => ProviderError::Unavailable,
        eagle_net::Error::Status(404) => ProviderError::NotFound,
        eagle_net::Error::Status(429) => ProviderError::RateLimited,
        eagle_net::Error::TooLarge => ProviderError::ApiChanged,
        _ => ProviderError::Network,
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
    })
}
fn tracks(v: &Value) -> Result<Vec<Track>, ProviderError> {
    v.as_array()
        .ok_or(ProviderError::ApiChanged)?
        .iter()
        .map(|t| parse_track(t.get("track").unwrap_or(t)))
        .collect()
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
impl Yandex {
    pub fn new(net: Arc<Client>) -> Self {
        Self {
            net,
            #[cfg(test)]
            api_base: None,
            token: RwLock::new(None),
            liked_ids: Mutex::new(None),
            uid: Mutex::new(None),
        }
    }
    fn token(&self) -> Result<Token, ProviderError> {
        if let Some(t) = self.token.read().map_err(|_| ProviderError::Auth)?.clone() {
            return Ok(t);
        }
        let token = Token::new(keyring()?.get_password().map_err(|_| ProviderError::Auth)?);
        *self.token.write().map_err(|_| ProviderError::Auth)? = Some(token.clone());
        Ok(token)
    }
    fn api(&self, path: &str, form: Option<Vec<(String, String)>>) -> Result<Value, ProviderError> {
        self.api_at(path, form, Priority::Metadata)
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
            tracks: if v["tracks"].is_array() {
                tracks(&v["tracks"])?
            } else {
                vec![]
            },
        })
    }
}
impl Provider for Yandex {
    fn name(&self) -> &str {
        "yandex"
    }
    fn login(&self, token: Token) -> Result<Account, ProviderError> {
        if token.expose().trim().is_empty() {
            return Err(ProviderError::Auth);
        }
        self.set_token(token.clone())?;
        let account = self.account()?;
        keyring()?
            .set_password(token.expose())
            .map_err(|_| ProviderError::Keyring)?;
        Ok(account)
    }
    fn account(&self) -> Result<Account, ProviderError> {
        let v = self.api("/account/status", None)?;
        Ok(Account {
            id: ident(&v["account"]["uid"])?,
            subscription: v["plus"]["hasPlus"].as_bool().unwrap_or(false),
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
            next: ((u64::from(page) + 1) * per < total).then(|| (page + 1).to_string()),
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
        let value = self.api(
            &format!("/users/{uid}/playlists/create"),
            Some(vec![
                ("title".into(), title.into()),
                ("visibility".into(), "private".into()),
            ]),
        )?;
        let playlist = self.parse_playlist(&value)?;
        if playlist
            .id
            .local()
            .split_once('_')
            .is_none_or(|(owner, kind)| {
                owner != uid || kind.is_empty() || !kind.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(ProviderError::ApiChanged);
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
        let result = self.api(
            &format!("/users/{uid}/playlists/{kind}/delete"),
            Some(Vec::new()),
        )?;
        if result.as_str() != Some("ok") {
            return Err(ProviderError::ApiChanged);
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
                Err(eagle_net::Error::Status(410)) if attempt == 0 => continue,
                result => result.map_err(net_error)?,
            };
            let url = direct_link(&response.bytes)?;
            return Ok(Stream {
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
    let target = if quality == Quality::High { 320 } else { 192 };
    let variant = v
        .as_array()
        .ok_or(ProviderError::ApiChanged)?
        .iter()
        .filter(|v| {
            v["codec"] == "mp3"
                && v["preview"] != true
                && v["bitrateInKbps"].as_u64().is_some_and(|b| b <= target)
        })
        .max_by_key(|v| v["bitrateInKbps"].as_u64())
        .ok_or(ProviderError::Unavailable)?;
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
    fn mock_api(responses: Vec<&'static str>) -> (Yandex, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
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
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
            requests
        });
        let mut provider = Yandex::new(Arc::new(Client::new().unwrap()));
        provider.api_base = Some(format!("http://{address}"));
        *provider.token.write().unwrap() = Some(Token::new("fixture-token".into()));
        *provider.uid.lock().unwrap() = Some("42".into());
        (provider, worker)
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
            Err(ProviderError::ApiChanged)
        ));
        assert!(matches!(
            provider.create_playlist("Bad"),
            Err(ProviderError::ApiChanged)
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
            Err(ProviderError::Unavailable)
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
