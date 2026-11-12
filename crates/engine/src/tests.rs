// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
fn fixture(name: &str) -> Source {
    Source::file(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
}
fn decoded(name: &str, rate: u32) -> Vec<[f32; 2]> {
    let source = fixture(name);
    let reader = source
        .open(Interrupt {
            generation: Arc::new(AtomicU32::new(0)),
            expected: 0,
            preload: false,
            seeking: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
    let mut pipeline = Pipeline::new(Decoder::open(reader).unwrap(), rate, Duration::ZERO).unwrap();
    let mut frames = Vec::new();
    loop {
        pipeline.fill().unwrap();
        frames.extend(pipeline.pending.drain(..));
        if pipeline.finished {
            if let Some(c) = &mut pipeline.converter {
                frames.extend(c.finish().unwrap());
            }
            break;
        }
    }
    frames
}
#[test]
fn all_v1_codecs_mono_and_stereo_at_both_rates_decode() {
    for rate in [44100, 48000] {
        for channels in [1, 2] {
            for extension in ["wav", "mp3", "aac", "m4a", "flac", "ogg", "opus"] {
                let name = format!("tone-{rate}-{channels}.{extension}");
                let result =
                    inspect(&fixture(&name), None).unwrap_or_else(|e| panic!("{name}: {e}"));
                let expected = result.input_rate as u64 / 4;
                if extension == "aac" {
                    assert!(
                        result.frames >= expected && result.frames < expected + 2048,
                        "{name}: {result:?}"
                    );
                } else {
                    assert_eq!(result.frames, expected, "{name}");
                }
                assert_eq!(result.damaged_packets, 0, "{name}");
            }
        }
    }
}
#[test]
fn multichannel_is_rejected_clearly() {
    let error = inspect(&fixture("multichannel.wav"), None).unwrap_err();
    assert!(error.to_string().contains("mono/stereo"));
}
#[test]
fn split_lossless_recording_is_identical_without_resampling() {
    let mut split = decoded("split-0.wav", 44100);
    split.extend(decoded("split-1.wav", 44100));
    assert_eq!(split, decoded("continuous.wav", 44100));
}
#[test]
fn accurate_seek_discards_to_requested_frame() {
    let original = decoded("continuous.wav", 44100);
    let source = fixture("continuous.wav");
    let reader = source
        .open(Interrupt {
            generation: Arc::new(AtomicU32::new(0)),
            expected: 0,
            preload: false,
            seeking: Arc::new(AtomicBool::new(false)),
        })
        .unwrap();
    let mut decoder = Decoder::open(reader).unwrap();
    decoder.seek(0.3).unwrap();
    let chunk = decoder.next().unwrap().unwrap();
    assert_eq!(&chunk.frames[..100], &original[13230..13330]);
}

fn queue_frames(names: &[&str], rate: u32) -> Vec<Frame> {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let mut worker = Worker::new(
        Config::default(),
        Arc::new(Shared::default()),
        commands,
        events,
    );
    worker.sources = names.iter().map(|n| fixture(n)).collect();
    let (output, mut consumer) = Output::offline(rate);
    worker.output = Some(output);
    let mut frames = Vec::new();
    while worker.decode_index < worker.sources.len() {
        if worker.pipeline.is_none() {
            worker.prepare().unwrap();
        }
        worker.produce(0).unwrap();
        while let Ok(frame) = consumer.pop() {
            frames.push(frame);
        }
    }
    frames
}
#[test]
fn continuous_split_recording_keeps_the_resampler_history_and_exact_track_positions() {
    let queued = queue_frames(&["split-0.wav", "split-1.wav"], 48000);
    let reference = decoded("continuous.wav", 48000);
    assert_eq!(queued.len(), reference.len());
    assert_eq!(
        queued.iter().map(|f| f.samples).collect::<Vec<_>>(),
        reference
    );
    assert_eq!(queued[23999].track, 0);
    assert_eq!(queued[23999].position, 23999);
    assert_eq!(queued[24000].track, 1);
    assert_eq!(queued[24000].position, 0);
}
#[test]
fn rate_changes_flush_the_old_tail_without_inserting_or_losing_frames() {
    let frames = queue_frames(
        &[
            "tone-44100-2.flac",
            "tone-48000-2.flac",
            "tone-44100-2.flac",
        ],
        48000,
    );
    assert_eq!(frames.len(), 36000);
    assert!(frames[..12000].iter().all(|f| f.track == 0));
    assert!(frames[12000..24000].iter().all(|f| f.track == 1));
    assert!(frames[24000..].iter().all(|f| f.track == 2));
}
#[test]
fn recovery_uses_heard_track_even_when_a_later_track_is_already_submitted() {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let shared = Arc::new(Shared::default());
    let mut worker = Worker::new(Config::default(), shared.clone(), commands, events);
    worker.output = Some(Output::offline(48000).0);
    worker.timeline = vec![(0, 0, 0), (48000, 1, 0)];
    shared.track.store(1, Ordering::Relaxed);
    shared.position.store(480, Ordering::Relaxed);
    shared.consumed.store(48480, Ordering::Relaxed);
    shared.audible_at.store(
        (shared.origin.elapsed() + Duration::from_millis(100)).as_nanos() as u64,
        Ordering::Relaxed,
    );
    let anchor = worker.anchor_value().unwrap();
    assert_eq!(anchor.track, 0);
    assert!(anchor.position.as_secs_f64() > 0.90 && anchor.position.as_secs_f64() < 0.92);
}

#[test]
fn lossy_split_matches_continuous_control_with_codec_tolerance() {
    let split = queue_frames(&["split-0.mp3", "split-1.mp3"], 44100);
    let control = decoded("continuous.mp3", 44100);
    assert_eq!(split.len(), control.len());
    let mse = split
        .iter()
        .zip(&control)
        .map(|(a, b)| (a.samples[0] - b[0]).powi(2) as f64)
        .sum::<f64>()
        / control.len() as f64;
    assert!(mse.sqrt() < 0.015, "RMS codec difference {}", mse.sqrt());
}

#[test]
fn seeking_lossy_audio_prerolls_decoder_state_to_the_continuous_reference() {
    for (name, seconds) in [("continuous.mp3", 0.7), ("tone-44100-2.m4a", 0.1)] {
        let original = decoded(name, 44100);
        let source = fixture(name);
        let reader = source
            .open(Interrupt {
                generation: Arc::new(AtomicU32::new(0)),
                expected: 0,
                preload: false,
                seeking: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let mut decoder = Decoder::open(reader).unwrap();
        decoder.seek(seconds).unwrap();
        let chunk = decoder.next().unwrap().unwrap();
        let at = (seconds * 44100.).round() as usize;
        let count = chunk.frames.len().min(200);
        let error = chunk.frames[..count]
            .iter()
            .zip(&original[at..at + count])
            .map(|(a, b)| (a[0] - b[0]).abs())
            .fold(0f32, f32::max);
        assert!(error < 0.0001, "{name}: seek error {error}");
    }
}

#[test]
fn pause_and_stop_remain_responsive_while_source_open_is_waiting() {
    let engine = Engine::new(Config::default()).unwrap();
    let (entered, opened) = mpsc::sync_channel(1);
    engine
        .enqueue(Source::new(move |interrupt| {
            let _ = entered.send(());
            while !interrupt.cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled").into())
        }))
        .unwrap();
    opened.recv_timeout(Duration::from_secs(2)).unwrap();
    engine.pause().unwrap();
    let start = Instant::now();
    let mut paused = false;
    while start.elapsed() < Duration::from_secs(2) {
        if matches!(
            engine.recv_timeout(Duration::from_millis(100)),
            Ok(Event::StateChanged(State::Paused))
        ) {
            paused = true;
            break;
        }
    }
    assert!(paused);
    engine.stop().unwrap();
    drop(engine);
    assert!(start.elapsed() < Duration::from_secs(2));
}
#[test]
fn buffering_anchor_is_sent_even_when_the_decoder_is_blocked() {
    let shared = Arc::new(Shared::default());
    shared
        .status
        .store(State::Playing as u32, Ordering::Release);
    shared.buffering.store(false, Ordering::Release);
    shared.rate.store(48000, Ordering::Release);
    shared.track.store(0, Ordering::Release);
    shared.position.store(48000, Ordering::Release);
    let (events, rx) = mpsc::sync_channel(128);
    let state = shared.clone();
    let observer = thread::spawn(move || observe(state, events));
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(Event::StateChanged(State::Playing))
    ));
    shared.buffering.store(true, Ordering::Release);
    let start = Instant::now();
    let mut anchor = None;
    while start.elapsed() < Duration::from_secs(3) {
        if let Ok(Event::Position(position)) = rx.recv_timeout(Duration::from_millis(100))
            && position.state == State::Buffering
        {
            anchor = Some(position);
            break;
        }
    }
    shared.observer_stop.store(true, Ordering::Release);
    observer.thread().unpark();
    observer.join().unwrap();
    assert_eq!(anchor.unwrap().position, Duration::from_secs(1));
}

#[test]
fn vbr_seek_with_and_without_an_index_matches_full_decoding() {
    for name in ["long-vbr.mp3", "long-vbr-no-index.mp3"] {
        let original = decoded(name, 44100);
        let source = fixture(name);
        let reader = source
            .open(Interrupt {
                generation: Arc::new(AtomicU32::new(0)),
                expected: 0,
                preload: false,
                seeking: Arc::new(AtomicBool::new(false)),
            })
            .unwrap();
        let mut decoder = Decoder::open(reader).unwrap();
        decoder.seek(5.1).unwrap();
        let chunk = decoder.next().unwrap().unwrap();
        let at = (5.1f64 * 44100.).round() as usize;
        let count = chunk.frames.len().min(200);
        let error = chunk.frames[..count]
            .iter()
            .zip(&original[at..at + count])
            .map(|(a, b)| (a[0] - b[0]).abs())
            .fold(0f32, f32::max);
        assert!(error < 0.0001, "{name}: seek error {error}");
        if name.contains("no-index") {
            assert_eq!(original.len() % 1152, 0);
        }
    }
}

#[test]
fn mixed_format_queue_has_exact_boundaries() {
    let names = [
        "tone-44100-1.mp3",
        "tone-48000-2.flac",
        "tone-44100-2.m4a",
        "tone-48000-1.ogg",
        "tone-44100-1.opus",
        "tone-48000-2.aac",
    ];
    let frames = queue_frames(&names, 48000);
    let mut offset = 0;
    for (track, name) in names.iter().enumerate() {
        let count = decoded(name, 48000).len();
        for (position, frame) in frames[offset..offset + count].iter().enumerate() {
            assert_eq!(
                (frame.track, frame.position),
                (track as u32, position as u64),
                "{name}"
            );
        }
        offset += count;
    }
    assert_eq!(frames.len(), offset);
}

#[test]
fn estimated_duration_never_truncates_long_vbr() {
    for name in ["long-vbr-no-index.mp3", "long-vbr.aac"] {
        let frames = decoded(name, 44100);
        assert!(frames.len() >= 44100 * 8, "{name}: {} frames", frames.len());
        assert!(
            frames.len() < 44100 * 8 + 4096,
            "{name}: {} frames",
            frames.len()
        );
    }
}

#[test]
fn repeat_and_order_keep_exact_frames_without_restarting() {
    for (order, repeat, expected) in [
        (vec![0, 1], Repeat::One, vec![0, 0, 0]),
        (vec![0, 1], Repeat::All, vec![0, 1, 0]),
        (vec![0, 2, 1], Repeat::Off, vec![0, 2, 1]),
    ] {
        let (_, commands) = mpsc::channel();
        let (events, _) = mpsc::sync_channel(128);
        let mut worker = Worker::new(
            Config::default(),
            Arc::new(Shared::default()),
            commands,
            events,
        );
        worker.sources = vec![
            fixture("tone-44100-2.flac"),
            fixture("tone-44100-2.flac"),
            fixture("tone-44100-2.flac"),
        ];
        worker.command(Command::Configure(order, repeat));
        let (output, mut consumer) = Output::offline(44100);
        worker.output = Some(output);
        let mut frames = Vec::new();
        while frames.len() < 33075 {
            if worker.pipeline.is_none() {
                worker.prepare().unwrap();
            }
            worker.produce(0).unwrap();
            while let Ok(frame) = consumer.pop() {
                frames.push(frame);
            }
        }
        let reference = decoded("tone-44100-2.flac", 44100);
        for (i, &id) in expected.iter().enumerate() {
            let section = &frames[i * 11025..(i + 1) * 11025];
            assert!(section.iter().all(|frame| frame.track == id));
            assert_eq!(
                section
                    .iter()
                    .map(|frame| frame.samples)
                    .collect::<Vec<_>>(),
                reference
            );
            assert_eq!(section[0].position, 0);
        }
    }
}

#[test]
fn live_order_change_preserves_current_frames_and_replaces_buffered_future() {
    for rate in [44100, 48000] {
        let (_, commands) = mpsc::channel();
        let (events, _) = mpsc::sync_channel(128);
        let shared = Arc::new(Shared::default());
        let mut worker = Worker::new(Config::default(), shared.clone(), commands, events);
        worker.sources = (0..3).map(|_| fixture("tone-44100-2.flac")).collect();
        let (output, mut consumer) = Output::offline(rate);
        worker.output = Some(output);
        while worker.decode_index < 2 {
            if worker.pipeline.is_none() {
                worker.prepare().unwrap();
            }
            worker.produce(0).unwrap();
        }
        for _ in 0..5000 {
            assert_eq!(consumer.pop().unwrap().track, 0);
        }
        shared.track.store(0, Ordering::Release);
        shared.consumed.store(5000, Ordering::Release);
        worker.command(Command::Configure(vec![0, 2, 1], Repeat::Off));
        let request = shared.policy_request.load(Ordering::Acquire);
        shared.policy_track.store(0, Ordering::Relaxed);
        shared.policy_consumed.store(5000, Ordering::Relaxed);
        shared
            .policy
            .store(u64::from(request) << 32, Ordering::Release);
        shared.policy_ack.store(request, Ordering::Release);
        worker.apply_policy();
        while worker.decode_index != 1 {
            if worker.pipeline.is_none() {
                worker.prepare().unwrap();
            }
            worker.produce(0).unwrap();
        }
        if rate == 48000 {
            worker.prepare().unwrap();
            worker.produce(0).unwrap();
        }
        let mut valid = Vec::new();
        while let Ok(frame) = consumer.pop() {
            if (frame.revision == (shared.policy.load(Ordering::Acquire) >> 32) as u32
                || frame.occurrence <= shared.policy.load(Ordering::Acquire) as u32 as u64)
                && frame.occurrence <= 1
            {
                valid.push(frame);
            }
        }
        let per_track = rate as usize / 4;
        assert_eq!(valid.len(), per_track * 2 - 5000, "rate {rate}");
        assert!(valid[..per_track - 5000].iter().all(|f| f.track == 0));
        assert!(valid[per_track - 5000..].iter().all(|f| f.track == 2));
        assert_eq!(worker.timeline[1].0, per_track as u64);
    }
}

#[test]
fn policy_change_preserves_unsubmitted_resampler_tail() {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let shared = Arc::new(Shared::default());
    let mut worker = Worker::new(Config::default(), shared.clone(), commands, events);
    worker.sources = (0..3).map(|_| fixture("tone-44100-2.flac")).collect();
    let (output, mut consumer) = Output::offline(48000);
    worker.output = Some(output);
    while worker.decode_index == 0 {
        if worker.pipeline.is_none() {
            worker.prepare().unwrap();
        }
        worker.produce(0).unwrap();
    }
    assert!(worker.tails.front().is_some_and(|tail| tail.2 > 0));
    worker.command(Command::Configure(vec![0, 2, 1], Repeat::Off));
    let request = shared.policy_request.load(Ordering::Acquire);
    shared.policy_track.store(0, Ordering::Relaxed);
    shared.policy_consumed.store(0, Ordering::Relaxed);
    shared
        .policy
        .store(u64::from(request) << 32, Ordering::Release);
    shared.policy_ack.store(request, Ordering::Release);
    worker.apply_policy();
    assert!(!worker.policy_tail.is_empty());
    while worker.decode_index != 1 {
        if worker.pipeline.is_none() {
            worker.prepare().unwrap();
        }
        worker.produce(0).unwrap();
    }
    let mut current = Vec::new();
    while let Ok(frame) = consumer.pop() {
        if frame.track == 0 {
            current.push(frame.samples);
        }
    }
    assert_eq!(current.len(), 12000);
    assert_eq!(current, decoded("tone-44100-2.flac", 48000));
}

#[test]
fn failed_sources_follow_order_and_stop_after_one_failed_cycle() {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let mut worker = Worker::new(
        Config::default(),
        Arc::new(Shared::default()),
        commands,
        events,
    );
    worker.sources = (0..3).map(|_| fixture("tone-44100-2.flac")).collect();
    worker.command(Command::Configure(vec![0, 2, 1], Repeat::All));
    worker.skip_failed();
    assert_eq!(worker.decode_index, 2);
    worker.skip_failed();
    assert_eq!(worker.decode_index, 1);
    worker.skip_failed();
    assert_eq!(worker.decode_index, worker.sources.len());
    worker.failures = 0;
    worker.decode_index = 0;
    worker.repeat = Repeat::One;
    worker.skip_failed();
    assert_eq!(worker.decode_index, 2);
}

#[test]
fn next_applies_pending_policy_and_preserves_pause() {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let shared = Arc::new(Shared::default());
    let mut worker = Worker::new(Config::default(), shared.clone(), commands, events);
    worker.sources = (0..3).map(|_| fixture("tone-44100-2.flac")).collect();
    worker.output = Some(Output::offline(44100).0);
    shared.paused.store(true, Ordering::Release);
    worker.command(Command::Configure(vec![0, 2, 1], Repeat::All));
    assert!(worker.pending_policy.is_some());
    shared.pending_interrupts.fetch_add(1, Ordering::AcqRel);
    worker.command(Command::Next);
    assert_eq!(worker.decode_index, 2);
    assert!(worker.pending_policy.is_none());
    assert!(shared.paused.load(Ordering::Acquire));
    assert_eq!(
        shared.policy_ack.load(Ordering::Acquire),
        shared.policy_request.load(Ordering::Acquire)
    );
    worker.command(Command::Configure(vec![0, 1, 2], Repeat::One));
    shared.pending_interrupts.fetch_add(1, Ordering::AcqRel);
    worker.command(Command::Seek(Duration::from_millis(100)));
    assert_eq!(worker.decode_index, 2);
    assert_eq!(worker.repeat, Repeat::One);
    assert!(worker.pending_policy.is_none());
    assert!(shared.paused.load(Ordering::Acquire));
}

#[test]
fn policy_change_preserves_each_protected_short_track_tail() {
    let (_, commands) = mpsc::channel();
    let (events, _) = mpsc::sync_channel(128);
    let shared = Arc::new(Shared::default());
    let mut worker = Worker::new(Config::default(), shared.clone(), commands, events);
    worker.sources = (0..3).map(|_| fixture("tone-44100-2.flac")).collect();
    worker.output = Some(Output::offline(44100).0);
    worker.decode_index = 2;
    worker.occurrence = 2;
    worker.prepare().unwrap();
    let pipeline = worker.pipeline.as_mut().unwrap();
    pipeline.pending.clear();
    pipeline.pending.extend([[0.1; 2], [0.2; 2], [0.3; 2]]);
    worker.tails.push_back((0, 10, 1, 0, 0));
    worker.tails.push_back((1, 20, 2, 1, 0));
    worker.command(Command::Configure(vec![0, 1, 2], Repeat::All));
    let request = shared.policy_request.load(Ordering::Acquire);
    shared.policy_track.store(1, Ordering::Relaxed);
    shared.policy_consumed.store(0, Ordering::Relaxed);
    shared
        .policy
        .store((u64::from(request) << 32) | 1, Ordering::Release);
    shared.policy_ack.store(request, Ordering::Release);
    worker.apply_policy();
    assert_eq!(
        worker
            .policy_tail
            .iter()
            .map(|t| (t.0, t.1, t.2, t.3))
            .collect::<Vec<_>>(),
        vec![
            ([0.1; 2], 0, 10, 0),
            ([0.2; 2], 1, 20, 1),
            ([0.3; 2], 1, 21, 1)
        ]
    );
}
