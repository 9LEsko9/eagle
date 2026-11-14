// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, unencrypted HLS VOD input. It is kept in memory and never cached.
use eagle_core::{SignedUrl, Stream};
use eagle_engine::{Interrupt, MediaSource, Source};
use eagle_net::{Priority, Request};
use eagle_provider_sdk::Http;
use std::{
    io::{Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};
use url::Url;

const MAX_PLAYLIST: usize = 1024 * 1024;
const MAX_SEGMENT: usize = 8 * 1024 * 1024;
const MAX_SEGMENTS: usize = 4096;

struct Segments {
    net: Arc<dyn Http>,
    urls: Vec<Url>,
    lengths: Vec<Option<usize>>,
    buffer: Vec<u8>,
    buffer_index: Option<usize>,
    position: u64,
    interrupt: Interrupt,
}
impl Segments {
    fn load(&mut self, index: usize) -> std::io::Result<()> {
        if self.buffer_index == Some(index) {
            return Ok(());
        }
        if self.interrupt.cancelled() {
            return Err(std::io::Error::other("playback cancelled"));
        }
        let segment = fetch(self.net.as_ref(), &self.urls[index], MAX_SEGMENT)
            .map_err(|_| std::io::Error::other("HLS segment unavailable"))?;
        if self.lengths[index].is_some_and(|n| n != segment.len()) {
            return Err(std::io::Error::other("HLS segment changed"));
        }
        self.lengths[index] = Some(segment.len());
        self.buffer = segment;
        self.buffer_index = Some(index);
        Ok(())
    }
}
impl Read for Segments {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        let mut offset = 0u64;
        for index in 0..self.urls.len() {
            if self.lengths[index].is_none() {
                self.load(index)?;
            }
            let len = self.lengths[index]
                .ok_or_else(|| std::io::Error::other("HLS segment unavailable"))?
                as u64;
            if self.position < offset + len {
                self.load(index)?;
                let start = (self.position - offset) as usize;
                let n = b.len().min(self.buffer.len() - start);
                b[..n].copy_from_slice(&self.buffer[start..start + n]);
                self.position += n as u64;
                return Ok(n);
            }
            offset += len;
        }
        Ok(0)
    }
}
impl Seek for Segments {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        let target = match p {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "HLS length is unknown",
                ));
            }
        };
        if target < 0 || target > u64::MAX as i128 {
            return Err(std::io::Error::other("invalid HLS seek"));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}
impl MediaSource for Segments {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn checked_url(base: &Url, value: &str) -> Result<Url, &'static str> {
    let url = base.join(value).map_err(|_| "invalid HLS URL")?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err("invalid HLS URL");
    }
    Ok(url)
}
fn playlist(base: &Url, text: &str) -> Result<Vec<Url>, &'static str> {
    if !text.lines().next().is_some_and(|s| s.trim() == "#EXTM3U") {
        return Err("invalid HLS playlist");
    }
    let mut urls = Vec::new();
    let mut end = false;
    for line in text.lines().map(str::trim) {
        if line == "#EXT-X-ENDLIST" {
            end = true;
        } else if line.starts_with("#EXT-X-KEY:") && line != "#EXT-X-KEY:METHOD=NONE" {
            return Err("encrypted HLS is unsupported");
        } else if line.starts_with("#EXT-X-BYTERANGE:")
            || line.starts_with("#EXT-X-STREAM-INF:")
            || line.starts_with("#EXT-X-DISCONTINUITY")
        {
            return Err("unsupported HLS playlist");
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attrs
                .split(',')
                .find_map(|part| {
                    part.trim()
                        .strip_prefix("URI=\"")
                        .and_then(|s| s.strip_suffix('"'))
                })
                .ok_or("invalid HLS map")?;
            if attrs.contains("BYTERANGE") {
                return Err("unsupported HLS map");
            }
            urls.push(checked_url(base, uri)?);
        } else if !line.is_empty() && !line.starts_with('#') {
            urls.push(checked_url(base, line)?);
        }
        if urls.len() > MAX_SEGMENTS {
            return Err("HLS playlist too large");
        }
    }
    if !end || urls.is_empty() {
        return Err("HLS must be a finite playlist");
    }
    Ok(urls)
}
fn fetch(net: &dyn Http, url: &Url, limit: usize) -> Result<Vec<u8>, eagle_engine::SourceError> {
    let mut req = Request::get(SignedUrl::new(url.as_str().into()));
    req.limit = limit;
    Ok(net.request(Priority::Playback, req, None)?.bytes)
}
pub(crate) fn source(
    net: Arc<dyn Http>,
    initial: Stream,
    provider: Arc<dyn eagle_core::Provider>,
    id: eagle_core::TrackId,
) -> Source {
    let initial = Mutex::new(Some(initial));
    Source::new(move |interrupt: Interrupt| {
        let stream = match initial.lock().map_err(|_| "HLS lock failed")?.take() {
            Some(stream) if stream.expires_at > std::time::SystemTime::now() => stream,
            _ => provider.resolve_stream(&id, eagle_core::Quality::High)?,
        };
        if stream.codec != "hls-aac" {
            return Err("unexpected stream format".into());
        }
        if !matches!(stream.encryption, eagle_core::Encryption::None) {
            return Err("encrypted HLS is unsupported".into());
        }
        let base = Url::parse(stream.url.expose()).map_err(|_| "invalid HLS URL")?;
        if base.scheme() != "https" || !base.username().is_empty() || base.password().is_some() {
            return Err("invalid HLS URL".into());
        }
        let body = fetch(net.as_ref(), &base, MAX_PLAYLIST)?;
        let urls = playlist(&base, std::str::from_utf8(&body)?)?;
        let lengths = vec![None; urls.len()];
        Ok(Box::new(Segments {
            net: net.clone(),
            urls,
            lengths,
            buffer: Vec::new(),
            buffer_index: None,
            position: 0,
            interrupt,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_provider_sdk::{HttpError, Response, Token};
    use std::sync::{Mutex, atomic::AtomicBool, atomic::AtomicU32};

    struct Fake {
        calls: Mutex<Vec<String>>,
    }
    impl Http for Fake {
        fn request(
            &self,
            _: Priority,
            req: Request,
            _: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
        ) -> Result<Response, HttpError> {
            let url = req.url.expose().to_owned();
            self.calls.lock().unwrap().push(url.clone());
            let bytes = if url.ends_with("one") {
                b"abc".to_vec()
            } else {
                b"def".to_vec()
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
    fn segment_reads_are_lazy_and_bounded_to_one_segment() {
        let net = Arc::new(Fake {
            calls: Mutex::new(Vec::new()),
        });
        let interrupt = Interrupt {
            generation: Arc::new(AtomicU32::new(0)),
            expected: 0,
            preload: false,
            seeking: Arc::new(AtomicBool::new(false)),
        };
        let urls = vec![
            Url::parse("https://example.com/one").unwrap(),
            Url::parse("https://example.com/two").unwrap(),
        ];
        let mut reader = Segments {
            net: net.clone(),
            urls,
            lengths: vec![None; 2],
            buffer: Vec::new(),
            buffer_index: None,
            position: 0,
            interrupt,
        };
        let mut a = [0; 3];
        reader.read_exact(&mut a).unwrap();
        assert_eq!(&a, b"abc");
        assert_eq!(net.calls.lock().unwrap().len(), 1);
        reader.read_exact(&mut a).unwrap();
        assert_eq!(&a, b"def");
        assert_eq!(reader.buffer.len(), 3);
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_exact(&mut a).unwrap();
        assert_eq!(&a, b"abc");
    }
    #[test]
    fn source_decodes_fragmented_mp4_through_engine() {
        struct AudioFixture;
        impl Http for AudioFixture {
            fn request(
                &self,
                _: Priority,
                req: Request,
                _: Option<&(dyn Fn() -> Result<Token, HttpError> + Sync)>,
            ) -> Result<Response, HttpError> {
                let data = include_bytes!("../../engine/tests/fixtures/hls-aac-fmp4.mp4");
                let moof = data
                    .windows(4)
                    .position(|window| window == b"moof")
                    .unwrap()
                    - 4;
                let bytes = if req.url.expose().ends_with("list") {
                    b"#EXTM3U\n#EXT-X-MAP:URI=\"init\"\n#EXTINF:2,\nsegment\n#EXT-X-ENDLIST\n"
                        .to_vec()
                } else if req.url.expose().ends_with("init") {
                    data[..moof].to_vec()
                } else {
                    data[moof..].to_vec()
                };
                Ok(Response {
                    status: 200,
                    bytes,
                    content_range: None,
                    etag: None,
                })
            }
        }
        let stream = Stream {
            url: SignedUrl::new("https://cdn.example/list".into()),
            codec: "hls-aac".into(),
            bitrate_kbps: Some(160),
            expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            encryption: eagle_core::Encryption::None,
        };
        let provider = Arc::new(eagle_provider_sdk::testkit::FakeProvider::new("fake"));
        let result = eagle_engine::inspect(
            &source(
                Arc::new(AudioFixture),
                stream,
                provider,
                "fake:1".parse().unwrap(),
            ),
            None,
        )
        .unwrap();
        assert!(result.frames > 80_000, "{:?}", result);
    }

    #[test]
    #[ignore = "uses the public SoundCloud website and audio network"]
    fn live_public_guest_track_decodes() {
        use eagle_core::Provider;
        let net = Arc::new(eagle_net::Client::new().unwrap());
        let provider = Arc::new(eagle_soundcloud::SoundCloud::new(net.clone()));
        let tracks = provider.search("field recording birds forest", 0).unwrap();
        let track = tracks
            .items
            .iter()
            .filter(|track| track.availability == eagle_core::Availability::Available)
            .min_by_key(|track| track.duration_ms)
            .expect("no freely playable candidate");
        eprintln!(
            "selected public candidate duration_ms={}",
            track.duration_ms
        );
        let stream = provider
            .resolve_stream(&track.id, eagle_core::Quality::High)
            .unwrap_or_else(|error| panic!("stream resolution failed: {error}"));
        let audio = source(net.clone(), stream, provider.clone(), track.id.clone());
        let result = eagle_engine::inspect(&audio, None)
            .unwrap_or_else(|error| panic!("decoding failed: {error}"));
        assert!(result.frames > 0);
        eprintln!("decoded {} frames of public audio", result.frames);
    }

    #[test]
    fn finite_playlist_resolves_map_and_rejects_unsupported_or_unsafe() {
        let base = Url::parse("https://cdn.example/path/list.m3u8?sig=secret").unwrap();
        let result = playlist(
            &base,
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1,\npart.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();
        assert_eq!(result[0].as_str(), "https://cdn.example/path/init.mp4");
        assert_eq!(result.len(), 2);
        for text in [
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\na\n#EXT-X-ENDLIST",
            "#EXTM3U\nhttp://evil.test/a\n#EXT-X-ENDLIST",
            "#EXTM3U\na\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:5\na\n#EXT-X-ENDLIST",
        ] {
            assert!(playlist(&base, text).is_err());
        }
    }
}
