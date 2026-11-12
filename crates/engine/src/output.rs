// SPDX-License-Identifier: GPL-3.0-or-later
//! Only this module runs on the realtime audio thread.
use crate::{Error, Shared};
use cpal::{
    FromSample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub samples: [f32; 2],
    pub position: u64,
    pub track: u32,
    pub generation: u32,
    pub occurrence: u64,
    pub revision: u32,
}
#[derive(Clone, Copy)]
pub(crate) struct Marker {
    pub track: u32,
    pub position: u64,
    pub at: u64,
}
pub(crate) struct Output {
    pub markers: Consumer<Marker>,
    _stream: Option<cpal::Stream>,
    pub producer: Producer<Frame>,
    pub rate: u32,
    pub device: String,
}
pub(crate) fn default_device_name() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}
impl Output {
    #[cfg(test)]
    pub fn offline(rate: u32) -> (Self, Consumer<Frame>) {
        let (producer, consumer) = RingBuffer::new(rate as usize * 2);
        let (_, markers) = RingBuffer::new(256);
        (
            Self {
                markers,
                _stream: None,
                producer,
                rate,
                device: String::new(),
            },
            consumer,
        )
    }

    pub fn open(preferred_rate: u32, seconds: u32, shared: Arc<Shared>) -> Result<Self, Error> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(Error::Device("нет устройства вывода".into()))?;
        let default = device
            .default_output_config()
            .map_err(|e| Error::Device(e.to_string()))?;
        let supported = device
            .supported_output_configs()
            .map_err(|e| Error::Device(e.to_string()))?
            .filter(|c| (1..=2).contains(&c.channels()))
            .collect::<Vec<_>>();
        let config = supported
            .iter()
            .find(|c| {
                c.channels() == 2
                    && c.min_sample_rate().0 <= preferred_rate
                    && c.max_sample_rate().0 >= preferred_rate
            })
            .or_else(|| {
                supported.iter().find(|c| {
                    c.min_sample_rate().0 <= preferred_rate
                        && c.max_sample_rate().0 >= preferred_rate
                })
            })
            .map(|c| (*c).with_sample_rate(cpal::SampleRate(preferred_rate)))
            .or_else(|| {
                if (1..=2).contains(&default.channels()) {
                    Some(default)
                } else {
                    supported.first().map(|c| (*c).with_max_sample_rate())
                }
            })
            .ok_or(Error::Unsupported("устройство не поддерживает mono/stereo"))?;
        let rate = config.sample_rate().0;
        let (producer, consumer) = RingBuffer::new(rate as usize * seconds as usize);
        let (renderer, markers) = Renderer::new(consumer, shared, rate, config.channels() as usize);
        let stream_config = config.config();
        let stream = match config.sample_format() {
            SampleFormat::I8 => build::<i8>(&device, &stream_config, renderer),
            SampleFormat::U8 => build::<u8>(&device, &stream_config, renderer),
            SampleFormat::U32 => build::<u32>(&device, &stream_config, renderer),
            SampleFormat::I64 => build::<i64>(&device, &stream_config, renderer),
            SampleFormat::U64 => build::<u64>(&device, &stream_config, renderer),
            SampleFormat::F32 => build::<f32>(&device, &stream_config, renderer),
            SampleFormat::I16 => build::<i16>(&device, &stream_config, renderer),
            SampleFormat::U16 => build::<u16>(&device, &stream_config, renderer),
            SampleFormat::I32 => build::<i32>(&device, &stream_config, renderer),
            SampleFormat::F64 => build::<f64>(&device, &stream_config, renderer),
            _ => return Err(Error::Unsupported("неподдерживаемый формат устройства")),
        }?;
        stream.play().map_err(|e| Error::Device(e.to_string()))?;
        Ok(Self {
            markers,
            _stream: Some(stream),
            producer,
            rate,
            device: device.name().unwrap_or_default(),
        })
    }
}
fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut renderer: Renderer,
) -> Result<cpal::Stream, Error> {
    let shared = renderer.shared.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let timestamp = info.timestamp();
                let latency = timestamp
                    .playback
                    .duration_since(&timestamp.callback)
                    .unwrap_or_default();
                let predicted =
                    renderer.shared.origin.elapsed().as_nanos() as u64 + latency.as_nanos() as u64;
                #[cfg(debug_assertions)]
                assert_no_alloc::assert_no_alloc(|| renderer.render(data, predicted));
                #[cfg(not(debug_assertions))]
                renderer.render(data, predicted);
            },
            move |_| {
                shared.interrupt_generation.fetch_or(1, Ordering::AcqRel);
            },
            None,
        )
        .map_err(|e| Error::Device(e.to_string()))
}
struct Renderer {
    consumer: Consumer<Frame>,
    shared: Arc<Shared>,
    rate: u32,
    channels: usize,
    gain: f32,
    generation: u32,
    markers: Producer<Marker>,
    last_track: u32,
    last_occurrence: u64,
    volume: f32,
    volume_target: f32,
    volume_step: f32,
    volume_left: u32,
}
impl Renderer {
    fn new(
        consumer: Consumer<Frame>,
        shared: Arc<Shared>,
        rate: u32,
        channels: usize,
    ) -> (Self, Consumer<Marker>) {
        let (markers, events) = RingBuffer::new(256);
        (
            Self {
                consumer,
                shared,
                rate,
                channels,
                gain: 0.,
                generation: 0,
                markers,
                last_track: u32::MAX,
                last_occurrence: u64::MAX,
                volume: 1.,
                volume_target: 1.,
                volume_step: 0.,
                volume_left: 0,
            },
            events,
        )
    }
    fn render<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T], predicted: u64) {
        self.shared.rendering.store(true, Ordering::SeqCst);
        struct Guard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _guard = Guard(&self.shared.rendering);
        if self.shared.resetting.load(Ordering::SeqCst) {
            data.fill(T::from_sample(0.));
            return;
        }
        let epoch = self.shared.generation.load(Ordering::Acquire);
        if epoch != self.generation {
            self.generation = epoch;
            self.gain = 0.;
            self.last_track = u32::MAX;
            self.last_occurrence = u64::MAX;
        }
        let request = self.shared.policy_request.load(Ordering::Acquire);
        if request != self.shared.policy_ack.load(Ordering::Relaxed) {
            let occurrence = if self.last_occurrence == u64::MAX {
                self.shared.reset_occurrence.load(Ordering::Relaxed)
            } else {
                self.last_occurrence
            };
            let track = if self.last_track == u32::MAX {
                self.shared.reset_track.load(Ordering::Relaxed)
            } else {
                self.last_track
            };
            self.shared.policy_track.store(track, Ordering::Relaxed);
            self.shared.policy_consumed.store(
                self.shared.consumed.load(Ordering::Acquire),
                Ordering::Relaxed,
            );
            self.shared.policy.store(
                (u64::from(request) << 32) | occurrence.min(u64::from(u32::MAX)),
                Ordering::Release,
            );
            self.shared.policy_ack.store(request, Ordering::Release);
        }
        let policy = self.shared.policy.load(Ordering::Acquire);
        while self
            .consumer
            .peek()
            .is_ok_and(|frame| frame.generation != epoch)
        {
            let _ = self.consumer.pop();
        }
        self.shared
            .callback_generation
            .store(epoch, Ordering::Release);
        let volume = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        if volume != self.volume_target {
            self.volume_target = volume;
            self.volume_left = (self.rate as f32 * 0.015).ceil() as u32;
            self.volume_step = (volume - self.volume) / self.volume_left as f32;
        }
        let step = 1.0 / (self.rate as f32 * 0.015);
        let mut buffering = self.shared.buffering.load(Ordering::Acquire);
        let paused = self.shared.paused.load(Ordering::Acquire);
        self.shared.callbacks.fetch_add(1, Ordering::Relaxed);
        let mut last = None;
        let mut consumed = 0u64;
        for (offset, output) in data.chunks_exact_mut(self.channels).enumerate() {
            if self.volume_left > 0 {
                self.volume += self.volume_step;
                self.volume_left -= 1;
                if self.volume_left == 0 {
                    self.volume = self.volume_target;
                }
            }
            let target = if paused || buffering { 0. } else { 1. };
            if self.gain < target {
                self.gain = (self.gain + step).min(target);
            } else {
                self.gain = (self.gain - step).max(target);
            }
            let mut value = [0.; 2];
            if self.gain > 0. || (!paused && !buffering) {
                let frame = loop {
                    match self.consumer.pop() {
                        Ok(frame)
                            if frame.generation == epoch && {
                                frame.revision == (policy >> 32) as u32
                                    || frame.occurrence <= (policy as u32) as u64
                            } =>
                        {
                            break Some(frame);
                        }
                        Ok(_) => continue,
                        Err(_) => break None,
                    }
                };
                if let Some(frame) = frame {
                    value = frame
                        .samples
                        .map(|x| (x * self.volume * self.gain).clamp(-1., 1.));
                    if frame.track != self.last_track || frame.occurrence != self.last_occurrence {
                        let _ = self.markers.push(Marker {
                            track: frame.track,
                            position: frame.position,
                            at: predicted + offset as u64 * 1_000_000_000 / self.rate as u64,
                        });
                        self.last_track = frame.track;
                        self.last_occurrence = frame.occurrence;
                    }
                    last = Some((
                        frame,
                        predicted + (offset as u64 + 1) * 1_000_000_000 / self.rate as u64,
                    ));
                    consumed += 1;
                } else if !self.shared.eof.load(Ordering::Acquire) && !buffering {
                    self.shared.underruns.fetch_add(1, Ordering::Relaxed);
                    self.shared.buffering.store(true, Ordering::Release);
                    buffering = true;
                }
            }
            if self.channels == 1 {
                output[0] = T::from_sample((value[0] + value[1]) * 0.5);
            } else {
                output[0] = T::from_sample(value[0]);
                output[1] = T::from_sample(value[1]);
            }
        }
        if let Some((frame, at)) = last {
            self.shared.sequence.fetch_add(1, Ordering::AcqRel);
            self.shared.frame_generation.store(epoch, Ordering::Relaxed);
            self.shared.track.store(frame.track, Ordering::Relaxed);
            self.shared
                .occurrence
                .store(frame.occurrence, Ordering::Relaxed);
            self.shared
                .position
                .store(frame.position + 1, Ordering::Relaxed);
            self.shared.audible_at.store(at, Ordering::Relaxed);
            self.shared.consumed.fetch_add(consumed, Ordering::Relaxed);
            self.shared.sequence.fetch_add(1, Ordering::Release);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_ack_after_reset_uses_the_new_track_and_occurrence() {
        use std::{sync::mpsc, time::Duration};

        for seek in [false, true] {
            for paused in [false, true] {
                let shared = Arc::new(Shared::default());
                let (_, commands) = mpsc::channel();
                let (events, _) = mpsc::sync_channel(128);
                let mut worker =
                    crate::Worker::new(crate::Config::default(), shared.clone(), commands, events);
                worker.sources = (0..3)
                    .map(|_| {
                        crate::Source::file(
                            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                                .join("tests/fixtures/tone-44100-2.flac"),
                        )
                    })
                    .collect();
                let (output, consumer) = Output::offline(44100);
                worker.output = Some(output);
                worker.occurrence = 7;
                let (mut renderer, mut markers) = Renderer::new(consumer, shared.clone(), 44100, 2);
                renderer.last_track = if seek { 1 } else { 0 };
                renderer.last_occurrence = 3;
                shared.track.store(renderer.last_track, Ordering::Relaxed);
                shared.consumed.store(500, Ordering::Relaxed);
                shared.paused.store(paused, Ordering::Relaxed);
                shared.pending_interrupts.fetch_add(1, Ordering::AcqRel);
                worker.command(if seek {
                    crate::Command::Seek(Duration::from_millis(100))
                } else {
                    crate::Command::Next
                });
                assert_eq!(worker.decode_index, 1);
                while worker.decode_index == 1 {
                    if worker.pipeline.is_none() {
                        worker.prepare().unwrap();
                    }
                    worker.produce(1).unwrap();
                }
                worker.command(crate::Command::Configure(vec![0, 1, 2], crate::Repeat::Off));
                renderer.render(&mut [0.0f32; 2], 0);
                worker.apply_policy();
                assert_eq!(worker.decode_index, 2);
                while worker.decode_index == 2 {
                    if worker.pipeline.is_none() {
                        worker.prepare().unwrap();
                    }
                    worker.produce(1).unwrap();
                }
                shared.eof.store(true, Ordering::Release);
                shared.buffering.store(false, Ordering::Release);
                shared.paused.store(false, Ordering::Release);
                renderer.gain = 1.;
                let current_frames = if seek { 6615 } else { 11025 };
                let mut samples = vec![0.0f32; (current_frames + 11025) * 2];
                renderer.render(&mut samples, 0);
                assert_eq!(
                    shared.consumed.load(Ordering::Relaxed),
                    500 + current_frames as u64 + 11025
                );
                assert_eq!(markers.pop().unwrap().track, 1);
                assert_eq!(markers.pop().unwrap().track, 2);
                assert!(markers.pop().is_err());
            }
        }
    }
    #[test]
    fn worker_refill_clears_underrun_without_external_flag_reset() {
        use std::{
            sync::mpsc,
            time::{Duration, Instant},
        };
        let shared = Arc::new(Shared::default());
        shared.buffering.store(false, Ordering::Release);
        let (tx, rx) = mpsc::channel();
        let (events, _) = mpsc::sync_channel(128);
        let (consumer_tx, consumer_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let state = shared.clone();
        let join = std::thread::spawn(move || {
            let (output, consumer) = Output::offline(44100);
            consumer_tx.send(consumer).unwrap();
            start_rx.recv().unwrap();
            let mut worker = crate::Worker::new(crate::Config::default(), state, rx, events);
            worker.sources.push(crate::Source::file(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/tone-44100-2.flac"),
            ));
            worker.output = Some(output);
            worker.run();
        });
        let consumer = consumer_rx.recv().unwrap();
        let (mut renderer, _) = Renderer::new(consumer, shared.clone(), 44100, 2);
        renderer.gain = 1.0;
        renderer.render(&mut [0.0f32; 2], 0);
        assert!(shared.buffering.load(Ordering::Acquire));
        start_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.buffering.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!shared.buffering.load(Ordering::Acquire));
        let mut data = [0.0f32; 64];
        renderer.render(&mut data, 0);
        assert!(shared.consumed.load(Ordering::Acquire) > 0);
        shared.pending_interrupts.fetch_add(1, Ordering::AcqRel);
        tx.send(crate::Command::Stop).unwrap();
        join.join().unwrap();
    }
    #[test]
    fn policy_change_discards_stale_future_but_keeps_current_and_new_frames() {
        let shared = Arc::new(Shared::default());
        shared.buffering.store(false, Ordering::Release);
        shared.policy.store(1u64 << 32, Ordering::Release);
        let (mut producer, consumer) = RingBuffer::new(16);
        for (sample, occurrence, revision) in [
            (0.1, 0, 0),
            (0.1, 0, 0),
            (0.9, 1, 0),
            (0.9, 1, 0),
            (0.2, 1, 1),
            (0.2, 1, 1),
        ] {
            producer
                .push(Frame {
                    samples: [sample; 2],
                    track: occurrence as u32,
                    position: 0,
                    generation: 0,
                    occurrence,
                    revision,
                })
                .unwrap();
        }
        let (mut renderer, mut markers) = Renderer::new(consumer, shared.clone(), 48_000, 2);
        renderer.gain = 1.0;
        let mut data = [0.0f32; 8];
        renderer.render(&mut data, 0);
        assert_eq!(data, [0.1, 0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2]);
        assert_eq!(markers.pop().unwrap().track, 0);
        assert_eq!(markers.pop().unwrap().track, 1);
        assert_eq!(shared.consumed.load(Ordering::Relaxed), 4);
    }
    #[test]
    fn changed_order_at_full_buffer_boundary_recovers_after_underrun() {
        let shared = Arc::new(Shared::default());
        shared.buffering.store(false, Ordering::Release);
        shared.policy.store(1u64 << 32, Ordering::Release);
        let (mut producer, consumer) = RingBuffer::new(16);
        for occurrence in std::iter::once(0).chain(std::iter::repeat_n(1, 15)) {
            producer
                .push(Frame {
                    samples: [0.5; 2],
                    position: 0,
                    track: occurrence as u32,
                    generation: 0,
                    occurrence,
                    revision: 0,
                })
                .unwrap();
        }
        let (mut renderer, _) = Renderer::new(consumer, shared.clone(), 48_000, 2);
        renderer.gain = 1.0;
        let mut data = [0.0f32; 4];
        renderer.render(&mut data, 0);
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(data, [0.5, 0.5, 0.0, 0.0]);
        producer
            .push(Frame {
                samples: [0.5; 2],
                position: 0,
                track: 2,
                generation: 0,
                occurrence: 1,
                revision: 1,
            })
            .unwrap();
        // The worker clears buffering once the refill threshold is met.
        shared.buffering.store(false, Ordering::Release);
        let mut next = [0.0f32; 2];
        renderer.render(&mut next, 0);
        assert_eq!(next, [0.5; 2]);
    }
    #[test]
    fn boundary_and_short_last_track_have_no_added_silence_or_ramp() {
        let shared = Arc::new(Shared::default());
        shared.buffering.store(false, Ordering::Relaxed);
        shared.eof.store(true, Ordering::Relaxed);
        let (mut producer, consumer) = RingBuffer::new(4000);
        for i in 0..3000 {
            producer
                .push(Frame {
                    samples: [0.5; 2],
                    position: i,
                    track: if i < 1500 { 0 } else { 1 },
                    generation: 0,
                    occurrence: 0,
                    revision: 0,
                })
                .unwrap();
        }
        let mut renderer = Renderer::new(consumer, shared.clone(), 48_000, 2).0;
        let mut samples = [0f32; 6000];
        assert_no_alloc::assert_no_alloc(|| renderer.render(&mut samples, 0));
        assert!(samples[1500..].iter().all(|s| (*s - 0.5).abs() < 0.0001));
        assert_eq!(shared.consumed.load(Ordering::Relaxed), 3000);
        assert_eq!(shared.track.load(Ordering::Relaxed), 1);
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn pause_and_underflow_do_not_advance_music_position() {
        let shared = Arc::new(Shared::default());
        shared.buffering.store(false, Ordering::Relaxed);
        let (_, consumer) = RingBuffer::new(128);
        let mut renderer = Renderer::new(consumer, shared.clone(), 48_000, 2).0;
        let mut samples = [1f32; 512];
        assert_no_alloc::assert_no_alloc(|| renderer.render(&mut samples, 0));
        assert!(samples.iter().all(|s| *s == 0.));
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(shared.consumed.load(Ordering::Relaxed), 0);
        shared.paused.store(true, Ordering::Relaxed);
        renderer.render(&mut samples, 0);
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
    }
}
