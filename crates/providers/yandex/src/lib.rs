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
    token: RwLock<Option<Token>>,
    liked_ids: Mutex<Option<Vec<String>>>,
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
            })
        })
        .collect()
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
            token: RwLock::new(None),
            liked_ids: Mutex::new(None),
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
        let mut request = Request::get(SignedUrl::new(format!("{API}{path}")));
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
    fn local<'a>(&self, id: &'a TrackId) -> Result<&'a str, ProviderError> {
        if id.provider() != "yandex" {
            return Err(ProviderError::InvalidId);
        }
        Ok(id.local())
    }
    fn parse_playlist(&self, v: &Value) -> Result<Playlist, ProviderError> {
        let owner = ident(&v["owner"]["uid"])?;
        let kind = ident(&v["kind"])?;
        Ok(Playlist {
            id: format!("yandex:{owner}_{kind}").parse()?,
            title: text(v, "title")?.into(),
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
        *self.token.write().map_err(|_| ProviderError::Auth)? = Some(token.clone());
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
        let uid = self.account()?.id;
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
            let uid = self.account()?.id;
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
        let uid = self.account()?.id;
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
        let uid = self.account()?.id;
        let action = if disliked { "add-multiple" } else { "remove" };
        self.api(
            &format!("/users/{uid}/dislikes/tracks/{action}"),
            Some(vec![("track-ids".into(), self.local(id)?.into())]),
        )?;
        Ok(())
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
