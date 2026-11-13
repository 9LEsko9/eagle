// SPDX-License-Identifier: GPL-3.0-or-later
//! Offline fixtures used by `cargo xtask bench`.
use eagle_core::*;
use std::{
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Fixture;
/// What the fixture implements; capability-driven routing reaches only these methods.
const fn capabilities() -> Capabilities {
    let mut c = Capabilities::none();
    c.search_tracks = true;
    c.likes = true;
    c.playlists_read = true;
    c
}
impl Provider for Fixture {
    fn info(&self) -> &ProviderInfo {
        static INFO: ProviderInfo = ProviderInfo {
            id: "fixture",
            name: "fixture",
            auth: AuthMethod::None,
            capabilities: capabilities(),
        };
        &INFO
    }
    fn account(&self) -> std::result::Result<Account, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn login(&self, _: Token) -> std::result::Result<Account, ProviderError> {
        self.account()
    }
    fn track(&self, _: &TrackId) -> std::result::Result<Track, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn album(&self, _: &TrackId) -> std::result::Result<Album, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn playlist(&self, _: &TrackId) -> std::result::Result<Playlist, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn search(&self, _: &str, _: u32) -> std::result::Result<Page<Track>, ProviderError> {
        Err(ProviderError::Unavailable)
    }
    fn playlists(&self, _: Option<&str>) -> std::result::Result<Page<Playlist>, ProviderError> {
        Ok(Page {
            items: vec![],
            next: None,
        })
    }
    fn likes(&self, cursor: Option<&str>) -> std::result::Result<Page<Track>, ProviderError> {
        let start = cursor
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| ProviderError::InvalidId)?;
        let end = (start + 500).min(10_000);
        let items = (start..end)
            .map(|n| {
                Ok(Track {
                    id: format!("fixture:{n}").parse()?,
                    title: format!("Fixture {n} {}", "metadata ".repeat(32)),
                    artists: vec![],
                    album: None,
                    album_title: None,
                    duration_ms: 1000,
                    cover: None,
                    availability: Availability::Available,
                    isrc: None,
                })
            })
            .collect::<std::result::Result<Vec<_>, ProviderError>>()?;
        Ok(Page {
            items,
            next: (end < 10_000).then(|| end.to_string()),
        })
    }
    fn set_liked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
        Ok(())
    }
    fn set_disliked(&self, _: &TrackId, _: bool) -> std::result::Result<(), ProviderError> {
        Ok(())
    }
    fn resolve_stream(
        &self,
        _: &TrackId,
        _: Quality,
    ) -> std::result::Result<Stream, ProviderError> {
        Err(ProviderError::Unavailable)
    }
}

fn settle(root: &Path, pass: usize) -> Result<()> {
    std::fs::write(root.join(format!("bench-pass-{pass}")), b"ready")?;
    let ack = root.join(format!("bench-ack-{pass}"));
    while !ack.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn percentile(values: &mut [f64], percent: usize) -> f64 {
    values.sort_by(f64::total_cmp);
    values[(values.len() - 1) * percent / 100]
}

/// Runs real storage/cover code without credentials or network, then parks for plateau sampling.
pub fn run(root: &Path, scenario: &str) -> Result<()> {
    std::fs::write(root.join("bench-ready"), b"ready")?;
    if scenario == "sync" {
        let store = eagle_store::Store::open(root)?;
        for pass in 0..3 {
            let started = Instant::now();
            crate::music::sync(&store, &Fixture)?;
            println!(
                "{}",
                serde_json::json!({"pass": pass, "elapsed_ms": started.elapsed().as_secs_f64()*1000.})
            );
            settle(root, pass)?;
        }
    } else {
        let (tx, rx) = mpsc::channel();
        let covers = crate::covers::Covers::start(
            root.to_owned(),
            Arc::new(eagle_net::Client::new()?),
            Arc::new(move |event| {
                if let crate::session::Event::Thumbnail(_, _) = event {
                    let _ = tx.send(());
                }
            }),
        )?;
        // One fixture file serves distinct keys, preserving the production decoded-cache workload.
        for pass in 0..3 {
            let started = Instant::now();
            let mut window_ms = Vec::new();
            let mut hot_window_ms = Vec::new();
            for (prefix, count, size) in [("t", 10_000, 64), ("a", 1000, 300)] {
                for start in (0..count).step_by(20) {
                    for latencies in [&mut window_ms, &mut hot_window_ms] {
                        let window_started = Instant::now();
                        covers.request(
                            (start..(start + 20).min(count))
                                .map(|n| crate::covers::Job {
                                    key: format!("{prefix}:fixture:{n}"),
                                    stem: "fixture".into(),
                                    uri: None,
                                    size,
                                })
                                .collect(),
                        );
                        for _ in start..(start + 20).min(count) {
                            rx.recv_timeout(Duration::from_secs(30))?;
                        }
                        latencies.push(window_started.elapsed().as_secs_f64() * 1000.);
                    }
                }
            }
            println!(
                "{}",
                serde_json::json!({"pass": pass, "elapsed_ms": started.elapsed().as_secs_f64()*1000.,
                    "window_p50_ms": percentile(&mut window_ms, 50),
                    "window_p95_ms": percentile(&mut window_ms, 95),
                    "hot_window_p50_ms": percentile(&mut hot_window_ms, 50),
                    "hot_window_p95_ms": percentile(&mut hot_window_ms, 95)})
            );
            settle(root, pass)?;
        }
        // Keep the service/cache alive while sampling the settled footprint.
        std::fs::write(root.join("bench-done"), b"done")?;
        loop {
            std::thread::park();
        }
    }
    std::fs::write(root.join("bench-done"), b"done")?;
    loop {
        std::thread::park();
    }
}
