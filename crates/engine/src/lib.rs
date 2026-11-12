// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded PCM engine. Providers and network operations live outside this crate.
mod decode;
mod mp4;
mod output;
mod resample;
mod source;
use decode::Decoder;
use output::{Frame, Output};
pub use source::{Interrupt, Reader, Source, SourceError};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
pub use symphonia::core::io::MediaSource;
#[cfg(test)]
mod tests;

#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ошибка чтения: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка декодирования: {0}")]
    Symphonia(#[from] symphonia::core::errors::Error),
    #[error("ошибка декодирования: {0}")]
    Decode(String),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("аудиоустройство: {0}")]
    Device(String),
    #[error("движок остановлен")]
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
    Buffering,
}
#[derive(Debug, Clone)]
pub struct PositionAnchor {
    pub track: u32,
    pub position: Duration,
    pub audible_at: Instant,
    pub state: State,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct Statistics {
    pub consumed_frames: u64,
    pub underruns: u64,
    pub callbacks: u64,
    pub damaged_packets: u64,
    pub output_rate: u32,
}
#[derive(Debug)]
pub enum Event {
    StateChanged(State),
    TrackChanged(u32),
    Position(PositionAnchor),
    Error { track: Option<u32>, message: String },
    Statistics(Statistics),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub buffer_seconds: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self { buffer_seconds: 2 }
    }
}
struct Shared {
    origin: Instant,
    generation: Arc<AtomicU32>,
    // Low bit invalidates the device; upper bits cancel decoder I/O for commands.
    interrupt_generation: Arc<AtomicU32>,
    paused: AtomicBool,
    buffering: AtomicBool,
    eof: AtomicBool,
    done: AtomicBool,
    resetting: AtomicBool,
    rendering: AtomicBool,
    callback_generation: AtomicU32,
    status: AtomicU32,
    rate: AtomicU32,
    pending_sources: AtomicU32,
    policy: AtomicU64,
    policy_request: AtomicU32,
    policy_ack: AtomicU32,
    policy_track: AtomicU32,
    policy_consumed: AtomicU64,
    occurrence: AtomicU64,
    pending_interrupts: AtomicU32,
    device: std::sync::Mutex<Option<String>>,
    observer_stop: AtomicBool,
    observer: std::sync::OnceLock<thread::Thread>,
    volume: AtomicU32,
    sequence: AtomicU64,
    track: AtomicU32,
    position: AtomicU64,
    audible_at: AtomicU64,
    consumed: AtomicU64,
    underruns: AtomicU64,
    callbacks: AtomicU64,
}
impl Default for Shared {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            generation: Arc::new(AtomicU32::new(0)),
            interrupt_generation: Arc::new(AtomicU32::new(0)),
            paused: AtomicBool::new(false),
            buffering: AtomicBool::new(true),
            eof: AtomicBool::new(false),
            done: AtomicBool::new(false),
            resetting: AtomicBool::new(false),
            rendering: AtomicBool::new(false),
            callback_generation: AtomicU32::new(0),
            status: AtomicU32::new(State::Idle as u32),
            rate: AtomicU32::new(0),
            pending_sources: AtomicU32::new(0),
            policy: AtomicU64::new(u32::MAX as u64),
            policy_request: AtomicU32::new(0),
            policy_ack: AtomicU32::new(0),
            policy_track: AtomicU32::new(u32::MAX),
            policy_consumed: AtomicU64::new(0),
            occurrence: AtomicU64::new(0),
            pending_interrupts: AtomicU32::new(0),
            device: std::sync::Mutex::new(None),
            observer_stop: AtomicBool::new(false),
            observer: std::sync::OnceLock::new(),
            volume: AtomicU32::new(1f32.to_bits()),
            sequence: AtomicU64::new(0),
            track: AtomicU32::new(u32::MAX),
            position: AtomicU64::new(0),
            audible_at: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
        }
    }
}
/// State notifications remain responsive while the decoder waits for source bytes.
fn observe(shared: Arc<Shared>, events: mpsc::SyncSender<Event>) {
    let mut previous = None;
    loop {
        let status = shared.status.load(Ordering::Acquire);
        let state = if status == State::Idle as u32 {
            State::Idle
        } else if shared.paused.load(Ordering::Acquire) {
            State::Paused
        } else if shared.buffering.load(Ordering::Acquire) {
            if status == State::Loading as u32 {
                State::Loading
            } else {
                State::Buffering
            }
        } else {
            State::Playing
        };
        if previous != Some(state) {
            previous = Some(state);
            let _ = events.try_send(Event::StateChanged(state));
            let rate = shared.rate.load(Ordering::Acquire);
            if rate > 0 {
                loop {
                    let seq = shared.sequence.load(Ordering::Acquire);
                    if !seq.is_multiple_of(2) {
                        thread::yield_now();
                        continue;
                    }
                    let track = shared.track.load(Ordering::Relaxed);
                    let frames = shared.position.load(Ordering::Relaxed);
                    let at = shared.audible_at.load(Ordering::Relaxed);
                    if shared.sequence.load(Ordering::Acquire) != seq {
                        continue;
                    }
                    if track != u32::MAX {
                        let _ = events.try_send(Event::Position(PositionAnchor {
                            track,
                            position: Duration::from_secs_f64(frames as f64 / rate as f64),
                            audible_at: shared.origin + Duration::from_nanos(at),
                            state,
                        }));
                    }
                    break;
                }
            }
        }
        if state != State::Idle {
            let expected = shared
                .device
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if expected.is_some() && output::default_device_name() != expected {
                shared.interrupt_generation.fetch_or(1, Ordering::AcqRel);
            }
        }
        if shared.observer_stop.load(Ordering::Acquire) {
            break;
        }
        if state != State::Idle {
            thread::park_timeout(Duration::from_secs(1));
        } else {
            thread::park();
        }
    }
}
enum Command {
    Enqueue(Source),
    Configure(Vec<usize>, Repeat),
    Pause(bool),
    Seek(Duration),
    Next,
    Volume(f32),
    Stop,
}
pub struct Engine {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<Event>,
    shared: Arc<Shared>,
    worker: Option<thread::JoinHandle<()>>,
    observer: Option<thread::JoinHandle<()>>,
}
impl Engine {
    pub fn new(config: Config) -> Result<Self, Error> {
        if !(2..=20).contains(&config.buffer_seconds) {
            return Err(Error::Unsupported("буфер должен быть от 2 до 20 секунд"));
        }
        let (commands, rx) = mpsc::channel();
        let (tx, events) = mpsc::sync_channel(128);
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let observer_events = tx.clone();
        let worker = thread::Builder::new()
            .name("eg-decoder".into())
            .spawn(move || Worker::new(config, state, rx, tx).run())?;
        let observed = shared.clone();
        let observer = match thread::Builder::new()
            .name("eg-control".into())
            .spawn(move || observe(observed, observer_events))
        {
            Ok(observer) => observer,
            Err(error) => {
                shared.pending_interrupts.fetch_add(1, Ordering::AcqRel);
                let _ = commands.send(Command::Stop);
                let _ = worker.join();
                return Err(error.into());
            }
        };
        let _ = shared.observer.set(observer.thread().clone());
        Ok(Self {
            observer: Some(observer),
            commands,
            events,
            shared,
            worker: Some(worker),
        })
    }
    fn send(&self, command: Command, interrupt: bool) -> Result<(), Error> {
        match &command {
            Command::Pause(paused) => self.shared.paused.store(*paused, Ordering::Release),
            Command::Volume(volume) => self
                .shared
                .volume
                .store(volume.to_bits(), Ordering::Release),
            _ => (),
        }
        if interrupt {
            self.shared
                .pending_interrupts
                .fetch_add(1, Ordering::AcqRel);
            self.shared
                .interrupt_generation
                .fetch_add(2, Ordering::AcqRel);
        }
        if self.commands.send(command).is_err() {
            if interrupt {
                self.shared
                    .pending_interrupts
                    .fetch_sub(1, Ordering::Release);
            }
            return Err(Error::Stopped);
        }
        if let Some(observer) = self.shared.observer.get() {
            observer.unpark();
        }
        Ok(())
    }
    pub fn enqueue(&self, source: Source) -> Result<(), Error> {
        self.shared.pending_sources.fetch_add(1, Ordering::AcqRel);
        self.shared.done.store(false, Ordering::Release);
        if let Err(error) = self.send(Command::Enqueue(source), false) {
            self.shared.pending_sources.fetch_sub(1, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
    /// Change future playback order without restarting the active source.
    pub fn configure_playback(&self, order: Vec<usize>, repeat: Repeat) -> Result<(), Error> {
        self.send(Command::Configure(order, repeat), false)
    }
    pub fn pause(&self) -> Result<(), Error> {
        self.send(Command::Pause(true), false)
    }
    pub fn resume(&self) -> Result<(), Error> {
        self.send(Command::Pause(false), false)
    }
    pub fn seek(&self, position: Duration) -> Result<(), Error> {
        self.send(Command::Seek(position), true)
    }
    pub fn next(&self) -> Result<(), Error> {
        self.send(Command::Next, true)
    }
    pub fn volume(&self, volume: f32) -> Result<(), Error> {
        if !volume.is_finite() {
            return Err(Error::Unsupported("громкость должна быть конечным числом"));
        }
        self.send(Command::Volume(volume.clamp(0., 1.)), false)
    }
    pub fn stop(&self) -> Result<(), Error> {
        self.send(Command::Stop, true)
    }
    pub fn finished(&self) -> bool {
        self.shared.pending_sources.load(Ordering::Acquire) == 0
            && self.shared.done.load(Ordering::Acquire)
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
    /// Transfer event consumption to a blocking dispatcher without polling the engine.
    pub fn take_events(&mut self) -> mpsc::Receiver<Event> {
        let (_, empty) = mpsc::channel();
        std::mem::replace(&mut self.events, empty)
    }
    pub fn try_recv(&self) -> Result<Event, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.shared.observer_stop.store(true, Ordering::Release);
        if let Some(observer) = self.observer.take() {
            observer.thread().unpark();
            let _ = observer.join();
        }
    }
}
struct Pipeline {
    decoder: Decoder,
    converter: Option<resample::Converter>,
    pending: VecDeque<[f32; 2]>,
    position: u64,
    finished: bool,
    finalized: bool,
}
impl Pipeline {
    fn new(decoder: Decoder, output_rate: u32, position: Duration) -> Result<Self, Error> {
        let converter = if decoder.rate() != output_rate {
            Some(resample::Converter::new(decoder.rate(), output_rate)?)
        } else {
            None
        };
        Ok(Self {
            decoder,
            converter,
            pending: VecDeque::new(),
            position: (position.as_secs_f64() * output_rate as f64) as u64,
            finished: false,
            finalized: false,
        })
    }
    fn fill(&mut self) -> Result<(), Error> {
        if self.finished || !self.pending.is_empty() {
            return Ok(());
        }
        match self.decoder.next()? {
            Some(chunk) => {
                if let Some(converter) = &mut self.converter {
                    self.pending.extend(converter.push(&chunk.frames)?);
                } else {
                    self.pending.extend(chunk.frames);
                }
            }
            None => {
                self.finished = true;
            }
        }
        Ok(())
    }
}
struct Worker {
    config: Config,
    shared: Arc<Shared>,
    commands: mpsc::Receiver<Command>,
    events: mpsc::SyncSender<Event>,
    sources: Vec<Source>,
    decode_index: usize,
    order: Vec<usize>,
    repeat: Repeat,
    occurrence: u64,
    revision: u32,
    next_revision: u32,
    pending_policy: Option<(u32, Vec<usize>, Repeat)>,
    pipeline: Option<Pipeline>,
    preload: Option<(usize, Decoder)>,
    preload_attempted: Option<usize>,
    output: Option<Output>,
    state: State,
    last_stats: Instant,
    restart_position: Duration,
    damaged: u64,
    failures: usize,
    stopping: bool,
    _power: Option<eagle_platform::power::PowerMonitor>,
    continuation: Option<resample::Converter>,
    tails: VecDeque<(u32, u64, u64, u64, u32)>,
    policy_tail: VecDeque<([f32; 2], u32, u64, u64, u32)>,
    seeking: Arc<AtomicBool>,
    seek_fallback: Option<(usize, Duration)>,
    timeline: Vec<(u64, u32, u64)>,
    produced_frames: u64,
    refilling: bool,
}
impl Worker {
    fn new(
        config: Config,
        shared: Arc<Shared>,
        commands: mpsc::Receiver<Command>,
        events: mpsc::SyncSender<Event>,
    ) -> Self {
        #[cfg(not(test))]
        let power = eagle_platform::power::PowerMonitor::new(shared.interrupt_generation.clone())
            .map_err(|e| log::debug!("power monitor: {e}"))
            .ok();
        #[cfg(test)]
        let power = None;
        Self {
            config,
            shared,
            commands,
            events,
            sources: Vec::new(),
            decode_index: 0,
            order: Vec::new(),
            repeat: Repeat::Off,
            occurrence: 0,
            revision: 0,
            next_revision: 0,
            pending_policy: None,
            pipeline: None,
            preload: None,
            preload_attempted: None,
            output: None,
            state: State::Idle,
            last_stats: Instant::now(),
            restart_position: Duration::ZERO,
            damaged: 0,
            failures: 0,
            stopping: false,
            _power: power,
            continuation: None,
            tails: VecDeque::new(),
            policy_tail: VecDeque::new(),
            seeking: Arc::new(AtomicBool::new(false)),
            seek_fallback: None,
            timeline: Vec::new(),
            produced_frames: 0,
            refilling: true,
        }
    }
    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }
    fn set_state(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.shared.status.store(state as u32, Ordering::Release);
            if let Some(observer) = self.shared.observer.get() {
                observer.unpark();
            }
            self.anchor();
        }
    }
    fn anchor_value(&self) -> Option<PositionAnchor> {
        let rate = self.output.as_ref()?.rate;
        loop {
            let seq = self.shared.sequence.load(Ordering::Acquire);
            if !seq.is_multiple_of(2) {
                continue;
            }
            let track = self.shared.track.load(Ordering::Relaxed);
            let frames = self.shared.position.load(Ordering::Relaxed);
            let at = self.shared.audible_at.load(Ordering::Relaxed);
            let consumed = self.shared.consumed.load(Ordering::Relaxed);
            if self.shared.sequence.load(Ordering::Acquire) != seq {
                continue;
            }
            if track == u32::MAX {
                return None;
            }
            let delivery = self.shared.origin + Duration::from_nanos(at);
            let now = Instant::now();
            let ahead =
                (delivery.saturating_duration_since(now).as_secs_f64() * rate as f64).ceil() as u64;
            let heard = consumed.saturating_sub(ahead);
            if let Some(&(start, id, position)) = self.timeline.iter().rev().find(|t| t.0 <= heard)
            {
                return Some(PositionAnchor {
                    track: id,
                    position: Duration::from_secs_f64(
                        (position + heard - start) as f64 / rate as f64,
                    ),
                    audible_at: now,
                    state: self.state,
                });
            }
            return Some(PositionAnchor {
                track,
                position: Duration::from_secs_f64(frames as f64 / rate as f64),
                audible_at: delivery,
                state: self.state,
            });
        }
    }
    fn anchor(&self) {
        if let Some(anchor) = self.anchor_value() {
            self.emit(Event::Position(anchor));
        }
    }
    fn audible_position(&self) -> Duration {
        self.anchor_value().map_or(self.restart_position, |a| {
            a.position
                .saturating_sub(a.audible_at.saturating_duration_since(Instant::now()))
        })
    }
    fn reset(&mut self, index: usize, position: Duration) {
        self.shared.resetting.store(true, Ordering::SeqCst);
        while self.shared.rendering.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        if let Some((revision, order, repeat)) = self.pending_policy.take() {
            self.order = order;
            self.repeat = repeat;
            self.revision = revision;
            self.shared.policy.store(
                (u64::from(revision) << 32) | u64::from(u32::MAX),
                Ordering::Release,
            );
            self.shared
                .policy_request
                .store(revision, Ordering::Release);
            self.shared.policy_ack.store(revision, Ordering::Release);
        }
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.timeline.clear();
        self.produced_frames = self.shared.consumed.load(Ordering::Acquire);
        self.refilling = true;
        self.continuation = None;
        self.tails.clear();
        self.policy_tail.clear();
        self.shared.buffering.store(true, Ordering::Release);
        self.shared.eof.store(false, Ordering::Release);
        self.decode_index = index;
        self.pipeline = None;
        self.preload = None;
        self.preload_attempted = None;
        self.restart_position = position;
        self.set_state(State::Loading);
        if index < self.sources.len() {
            self.emit(Event::Position(PositionAnchor {
                track: index as u32,
                position,
                audible_at: Instant::now(),
                state: State::Loading,
            }));
        }
        self.shared.resetting.store(false, Ordering::SeqCst);
    }
    fn successor(&self, index: usize, automatic: bool) -> Option<usize> {
        if automatic && self.repeat == Repeat::One {
            return Some(index);
        }
        if self.order.is_empty() {
            return (index + 1 < self.sources.len())
                .then_some(index + 1)
                .or_else(|| (self.repeat == Repeat::All && !self.sources.is_empty()).then_some(0));
        }
        let pos = self.order.iter().position(|&item| item == index)?;
        self.order
            .get(pos + 1)
            .copied()
            .or_else(|| (self.repeat == Repeat::All).then(|| self.order[0]))
    }
    fn advance(&mut self) {
        self.decode_index = self
            .successor(self.decode_index, true)
            .unwrap_or(self.sources.len());
        self.occurrence += 1;
    }
    fn skip_failed(&mut self) {
        self.failures += 1;
        self.decode_index = if self.failures >= self.sources.len() {
            self.sources.len()
        } else {
            self.successor(self.decode_index, false)
                .unwrap_or(self.sources.len())
        };
        self.occurrence += 1;
    }
    fn apply_policy(&mut self) {
        let Some((revision, _, _)) = self.pending_policy.as_ref() else {
            return;
        };
        if self.shared.policy_ack.load(Ordering::Acquire) != *revision {
            return;
        }
        let Some((revision, order, repeat)) = self.pending_policy.take() else {
            return;
        };
        let current = self.shared.policy_track.load(Ordering::Relaxed);
        let current = if current == u32::MAX {
            self.timeline
                .first()
                .map_or(self.decode_index, |entry| entry.1 as usize)
        } else {
            current as usize
        };
        let occurrence = (self.shared.policy.load(Ordering::Acquire) as u32) as u64;
        let consumed = self.shared.policy_consumed.load(Ordering::Relaxed);
        self.order = order;
        self.repeat = repeat;
        self.revision = revision;
        self.preload = None;
        self.preload_attempted = None;
        if self.occurrence > occurrence || self.decode_index >= self.sources.len() {
            let boundary = self
                .timeline
                .iter()
                .rposition(|entry| entry.0 < consumed)
                .unwrap_or(0)
                + 1;
            if let Some(entry) = self.timeline.get(boundary) {
                self.produced_frames = entry.0;
                self.timeline.truncate(boundary);
            }
            self.decode_index = self.successor(current, true).unwrap_or(self.sources.len());
            self.occurrence = occurrence + 1;
            if self.tails.front().is_some_and(|tail| tail.3 <= occurrence) {
                let mut available = self
                    .pipeline
                    .as_mut()
                    .map(|pipeline| std::mem::take(&mut pipeline.pending))
                    .unwrap_or_default();
                let converter = self
                    .pipeline
                    .as_mut()
                    .and_then(|pipeline| pipeline.converter.take())
                    .or_else(|| self.continuation.take());
                if let Some(mut converter) = converter {
                    match converter.finish() {
                        Ok(frames) => available.extend(frames),
                        Err(error) => {
                            log::warn!(error = error.to_string().as_str(); "resampler tail unavailable")
                        }
                    }
                }
                while self.tails.front().is_some_and(|tail| tail.3 <= occurrence) {
                    let Some((track, mut position, mut remaining, tail_occurrence, tail_revision)) =
                        self.tails.pop_front()
                    else {
                        break;
                    };
                    while remaining > 0 {
                        let Some(samples) = available.pop_front() else {
                            break;
                        };
                        self.policy_tail.push_back((
                            samples,
                            track,
                            position,
                            tail_occurrence,
                            tail_revision,
                        ));
                        position += 1;
                        remaining -= 1;
                    }
                }
            }
            self.pipeline = None;
            self.continuation = None;
            self.tails.clear();
            self.shared.eof.store(false, Ordering::Release);
            self.shared.done.store(false, Ordering::Release);
        }
    }
    fn command(&mut self, cmd: Command) {
        if matches!(cmd, Command::Seek(_) | Command::Next | Command::Stop) {
            self.shared
                .pending_interrupts
                .fetch_sub(1, Ordering::AcqRel);
        }
        match cmd {
            Command::Stop => self.stopping = true,
            Command::Enqueue(source) => {
                self.sources.push(source);
                self.shared.done.store(false, Ordering::Release);
                self.shared.pending_sources.fetch_sub(1, Ordering::Release);
                if self.state == State::Idle {
                    self.set_state(State::Loading);
                }
                self.shared.eof.store(false, Ordering::Release);
            }
            Command::Configure(order, repeat) => {
                if order.iter().any(|&id| id >= self.sources.len()) {
                    return;
                }
                self.next_revision = self.next_revision.wrapping_add(1);
                self.pending_policy = Some((self.next_revision, order, repeat));
                if self.output.is_none() {
                    self.shared
                        .policy_track
                        .store(self.decode_index as u32, Ordering::Relaxed);
                    self.shared.policy_consumed.store(
                        self.shared.consumed.load(Ordering::Acquire),
                        Ordering::Relaxed,
                    );
                    self.shared.policy.store(
                        (u64::from(self.next_revision) << 32)
                            | self.occurrence.min(u64::from(u32::MAX)),
                        Ordering::Release,
                    );
                    self.shared
                        .policy_ack
                        .store(self.next_revision, Ordering::Release);
                }
                self.shared
                    .policy_request
                    .store(self.next_revision, Ordering::Release);
                self.apply_policy();
            }
            Command::Pause(paused) => {
                self.shared.paused.store(paused, Ordering::Release);
                self.set_state(if paused {
                    State::Paused
                } else {
                    State::Buffering
                });
                self.anchor();
            }
            Command::Volume(volume) => {
                self.shared
                    .volume
                    .store(volume.to_bits(), Ordering::Relaxed);
            }
            Command::Seek(position) => {
                let index = self
                    .anchor_value()
                    .map_or(self.decode_index, |a| a.track as usize);
                let previous = self.audible_position();
                self.reset(index, position);
                self.seek_fallback = Some((index, previous));
            }
            Command::Next => {
                if let Some((_, order, repeat)) = &self.pending_policy {
                    self.order = order.clone();
                    self.repeat = *repeat;
                }
                let current = self
                    .anchor_value()
                    .map_or(self.decode_index, |a| a.track as usize);
                let index = self.successor(current, false).unwrap_or(self.sources.len());
                self.reset(index.min(self.sources.len()), Duration::ZERO);
            }
        }
    }
    fn interrupted(&self, epoch: u32) -> bool {
        let current = self.shared.interrupt_generation.load(Ordering::Acquire);
        current != epoch
            || current & 1 != 0
            || self.shared.pending_interrupts.load(Ordering::Acquire) > 0
    }
    fn open_decoder(&self, index: usize) -> Result<Decoder, Error> {
        let interrupt = Interrupt {
            generation: self.shared.interrupt_generation.clone(),
            expected: self.shared.interrupt_generation.load(Ordering::Acquire),
            preload: index > self.decode_index,
            seeking: self.seeking.clone(),
        };
        if interrupt.expected & 1 != 0 || self.shared.pending_interrupts.load(Ordering::Acquire) > 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "команда ожидает применения",
            )
            .into());
        }
        let reader = self.sources[index]
            .open(interrupt)
            .map_err(|e| Error::Decode(e.to_string()))?;
        Decoder::open(reader)
    }
    fn prepare(&mut self) -> Result<(), Error> {
        self.seeking
            .store(!self.restart_position.is_zero(), Ordering::Release);
        if self.output.is_none() && self.state != State::Buffering {
            self.set_state(State::Loading);
        }
        let mut decoder = if let Some((index, decoder)) = self.preload.take() {
            if index == self.decode_index {
                drop(decoder);
                self.open_decoder(self.decode_index)?
            } else {
                self.open_decoder(self.decode_index)?
            }
        } else {
            self.open_decoder(self.decode_index)?
        };
        if self.output.is_none() {
            self.output = Some(Output::open(
                decoder.rate(),
                self.config.buffer_seconds,
                self.shared.clone(),
            )?);
        }
        let rate = self.output.as_ref().ok_or(Error::Stopped)?.rate;
        self.shared.rate.store(rate, Ordering::Release);
        *self.shared.device.lock().unwrap_or_else(|p| p.into_inner()) =
            self.output.as_ref().map(|o| o.device.clone());
        let position = self.restart_position;
        if !position.is_zero() && decoder.seek(position.as_secs_f64()).is_err() {
            decoder = self.open_decoder(self.decode_index)?;
            let mut skip = (position.as_secs_f64() * decoder.rate() as f64) as usize;
            while skip > 0 {
                let Some(mut chunk) = decoder.next()? else {
                    break;
                };
                if chunk.frames.len() > skip {
                    chunk.frames.drain(..skip);
                    let mut pipeline = Pipeline::new(decoder, rate, position)?;
                    if let Some(converter) = &mut pipeline.converter {
                        pipeline.pending.extend(converter.push(&chunk.frames)?);
                    } else {
                        pipeline.pending.extend(chunk.frames);
                    }
                    self.pipeline = Some(pipeline);
                    self.restart_position = Duration::ZERO;
                    self.seeking.store(false, Ordering::Release);
                    self.seek_fallback = None;
                    return Ok(());
                }
                skip -= chunk.frames.len();
            }
        }
        let mut pipeline = Pipeline::new(decoder, rate, position)?;
        if let Some(converter) = self.continuation.take() {
            pipeline.converter = Some(converter);
        }
        pipeline.fill()?;
        self.pipeline = Some(pipeline);
        self.restart_position = Duration::ZERO;
        Ok(())
    }
    fn produce(&mut self, epoch: u32) -> Result<(), Error> {
        if let Some(output) = &mut self.output {
            while output.producer.slots() > 0 {
                let Some((samples, track, position, occurrence, revision)) =
                    self.policy_tail.pop_front()
                else {
                    break;
                };
                if position == 0 || self.timeline.last().is_none_or(|entry| entry.1 != track) {
                    self.timeline.push((self.produced_frames, track, position));
                }
                self.produced_frames += 1;
                let _ = output.producer.push(Frame {
                    samples,
                    track,
                    position,
                    generation: epoch,
                    occurrence,
                    revision,
                });
            }
            if !self.policy_tail.is_empty() {
                return Ok(());
            }
        }
        if let (Some(pipeline), Some(output)) = (&mut self.pipeline, &mut self.output) {
            if output.producer.slots() == 0 {
                return Ok(());
            }
            pipeline.fill()?;
            if !pipeline.pending.is_empty() || pipeline.finished {
                self.seeking.store(false, Ordering::Release);
                self.seek_fallback = None;
            }
            while output.producer.slots() > 0 {
                let Some(samples) = pipeline.pending.pop_front() else {
                    break;
                };
                let (track, position, occurrence, revision) =
                    if let Some(tail) = self.tails.front_mut() {
                        let label = (tail.0, tail.1, tail.3, tail.4);
                        tail.1 += 1;
                        tail.2 -= 1;
                        if tail.2 == 0 {
                            self.tails.pop_front();
                        }
                        label
                    } else {
                        let p = pipeline.position;
                        pipeline.position += 1;
                        (self.decode_index as u32, p, self.occurrence, self.revision)
                    };
                if position == 0 || self.timeline.last().is_none_or(|t| t.1 != track) {
                    self.timeline.push((self.produced_frames, track, position));
                }
                self.produced_frames += 1;
                let _ = output.producer.push(Frame {
                    samples,
                    position,
                    track,
                    generation: epoch,
                    occurrence,
                    revision,
                });
            }
        }
        if self
            .pipeline
            .as_ref()
            .is_some_and(|p| p.finished && p.pending.is_empty())
        {
            let needs_flush = self
                .pipeline
                .as_ref()
                .is_some_and(|p| p.converter.is_some() && !p.finalized);
            if needs_flush {
                if let Some(next) = self.successor(self.decode_index, true)
                    && self.preload.is_none()
                    && let Ok(decoder) = self.open_decoder(next)
                {
                    self.preload = Some((next, decoder));
                }
                let same_rate = self.preload.as_ref().is_some_and(|(_, d)| {
                    self.pipeline
                        .as_ref()
                        .is_some_and(|p| d.rate() == p.decoder.rate())
                });
                let pipeline = self.pipeline.as_mut().ok_or(Error::Stopped)?;
                if same_rate {
                    let converter = pipeline.converter.take().ok_or(Error::Stopped)?;
                    let owed = converter
                        .remaining()
                        .saturating_sub(self.tails.iter().map(|t| t.2).sum());
                    if owed > 0 {
                        self.tails.push_back((
                            self.decode_index as u32,
                            pipeline.position,
                            owed,
                            self.occurrence,
                            self.revision,
                        ));
                    }
                    self.continuation = Some(converter);
                } else {
                    pipeline.pending.extend(
                        pipeline
                            .converter
                            .as_mut()
                            .ok_or(Error::Stopped)?
                            .finish()?,
                    );
                    pipeline.finalized = true;
                    return Ok(());
                }
            }
            let pipeline = self.pipeline.take().ok_or(Error::Stopped)?;
            self.damaged += pipeline.decoder.damaged();
            self.advance();
        }
        Ok(())
    }
    fn run(&mut self) {
        loop {
            while let Ok(command) = self.commands.try_recv() {
                self.command(command);
            }
            if self.stopping {
                break;
            }
            self.apply_policy();
            if self.sources.is_empty() {
                match self.commands.recv() {
                    Ok(command) => self.command(command),
                    Err(_) => break,
                }
                continue;
            }
            let urgent = self
                .shared
                .interrupt_generation
                .fetch_and(!1, Ordering::AcqRel)
                & 1
                != 0;
            if urgent {
                let (index, position) = if self.pipeline.is_none()
                    && matches!(self.state, State::Loading | State::Buffering)
                {
                    (self.decode_index, self.restart_position)
                } else {
                    (
                        self.anchor_value()
                            .map_or(self.decode_index, |a| a.track as usize),
                        self.audible_position(),
                    )
                };
                self.output = None;
                *self.shared.device.lock().unwrap_or_else(|p| p.into_inner()) = None;
                self.reset(index, position);
                self.set_state(State::Buffering);
            }
            if self.shared.pending_interrupts.load(Ordering::Acquire) > 0 {
                self.wait(Duration::from_millis(1));
                continue;
            }
            let epoch = self.shared.interrupt_generation.load(Ordering::Acquire);
            if self.pipeline.is_none()
                && self.decode_index < self.sources.len()
                && let Err(error) = self.prepare()
            {
                if self.interrupted(epoch) {
                    continue;
                }
                self.emit(Event::Error {
                    track: Some(self.decode_index as u32),
                    message: error.to_string(),
                });
                if let Some((index, position)) = self.seek_fallback.take() {
                    self.reset(index, position);
                    continue;
                }
                if matches!(error, Error::Device(_)) {
                    self.set_state(State::Buffering);
                    self.wait(Duration::from_secs(1));
                    continue;
                }
                self.skip_failed();
                continue;
            }
            if let Some(output) = &mut self.output {
                let capacity = output.producer.buffer().capacity();
                let free = output.producer.slots();
                if capacity - free <= capacity / 2 {
                    self.refilling = true;
                }
                if free == 0 {
                    self.refilling = false;
                }
            }
            let before_produce = self.produced_frames;
            if self.refilling
                && let Err(error) = self.produce(self.shared.generation.load(Ordering::Acquire))
            {
                if self.interrupted(epoch) {
                    continue;
                }
                self.emit(Event::Error {
                    track: Some(self.decode_index as u32),
                    message: error.to_string(),
                });
                if let Some((index, position)) = self.seek_fallback.take() {
                    self.reset(index, position);
                    continue;
                }
                self.pipeline = None;
                self.skip_failed();
                continue;
            }
            if self.produced_frames > before_produce {
                self.failures = 0;
            }
            let eof = self.decode_index >= self.sources.len();
            self.shared.eof.store(eof, Ordering::Release);
            if let Some(output) = &mut self.output {
                let buffered = output.producer.buffer().capacity() - output.producer.slots();
                if self.shared.buffering.load(Ordering::Acquire)
                    && self.shared.callback_generation.load(Ordering::Acquire)
                        == self.shared.generation.load(Ordering::Acquire)
                    && (buffered >= output.rate as usize * 2 || (eof && buffered > 0))
                {
                    self.shared.buffering.store(false, Ordering::Release);
                }
                let paused = self.shared.paused.load(Ordering::Acquire);
                let buffering = self.shared.buffering.load(Ordering::Acquire);
                if eof && buffered == 0 {
                    let pending = self.anchor_value().map_or(Duration::ZERO, |a| {
                        a.audible_at.saturating_duration_since(Instant::now())
                    });
                    if !pending.is_zero() {
                        self.wait(pending.min(Duration::from_millis(100)));
                        continue;
                    }
                    self.idle();
                    continue;
                }
                self.set_state(if paused {
                    State::Paused
                } else if buffering {
                    State::Buffering
                } else {
                    State::Playing
                });
            } else if eof {
                self.idle();
                continue;
            }
            while let Some(marker) = self.output.as_mut().and_then(|o| o.markers.pop().ok()) {
                self.emit(Event::TrackChanged(marker.track));
                if let Some(output) = &self.output {
                    self.emit(Event::Position(PositionAnchor {
                        track: marker.track,
                        position: Duration::from_secs_f64(
                            marker.position as f64 / output.rate as f64,
                        ),
                        audible_at: self.shared.origin + Duration::from_nanos(marker.at),
                        state: self.state,
                    }));
                }
            }
            if let Some(next) = self.successor(self.decode_index, true)
                && self.preload_attempted != Some(next)
            {
                let ready = self.pipeline.as_ref().is_some_and(|p| {
                    p.decoder.duration().is_some_and(|d| {
                        d - p.position as f64
                            / self.output.as_ref().map_or(48_000, |o| o.rate) as f64
                            <= 30.
                    })
                });
                if ready {
                    self.preload_attempted = Some(next);
                    if let Ok(d) = self.open_decoder(next) {
                        self.preload = Some((next, d));
                    }
                }
            }
            let consumed = self.shared.consumed.load(Ordering::Acquire);
            let keep_from =
                consumed.saturating_sub(self.output.as_ref().map_or(0, |o| u64::from(o.rate) * 10));
            if let Some(index) = self.timeline.iter().rposition(|entry| entry.0 <= keep_from)
                && index > 0
            {
                self.timeline.drain(..index);
            }
            if self.last_stats.elapsed() >= Duration::from_secs(1) {
                self.last_stats = Instant::now();
                self.emit_stats();
                self.anchor();
            }
            let delay = self
                .output
                .as_mut()
                .map_or(Duration::from_millis(100), |o| {
                    if self.shared.callback_generation.load(Ordering::Acquire)
                        != self.shared.generation.load(Ordering::Acquire)
                    {
                        return Duration::from_millis(10);
                    }
                    if !self.refilling && !eof {
                        return Duration::from_millis(10);
                    }
                    let free = o.producer.slots();
                    let buffered = o.producer.buffer().capacity() - free;
                    if free == 0 || eof {
                        Duration::from_secs_f64(
                            buffered.saturating_sub(o.rate as usize) as f64 / o.rate as f64,
                        )
                        .clamp(Duration::from_millis(10), Duration::from_secs(1))
                    } else {
                        Duration::ZERO
                    }
                });
            if !delay.is_zero() {
                self.wait(delay);
            }
        }
        self.emit_stats();
        self.set_state(State::Idle);
        self.output = None;
        self.shared.done.store(true, Ordering::Release);
    }
    fn idle(&mut self) {
        self.emit_stats();
        self.set_state(State::Idle);
        self.output = None;
        *self.shared.device.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.timeline.clear();
        self.shared.track.store(u32::MAX, Ordering::Release);
        self.shared.done.store(true, Ordering::Release);
        match self.commands.recv() {
            Ok(command) => self.command(command),
            Err(_) => self.stopping = true,
        }
    }
    fn wait(&mut self, duration: Duration) {
        if let Ok(command) = self.commands.recv_timeout(duration) {
            self.command(command);
        }
    }
    fn emit_stats(&self) {
        let stats = Statistics {
            output_rate: self.output.as_ref().map_or(0, |o| o.rate),
            consumed_frames: self.shared.consumed.load(Ordering::Relaxed),
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            callbacks: self.shared.callbacks.load(Ordering::Relaxed),
            damaged_packets: self.damaged
                + self.pipeline.as_ref().map_or(0, |p| p.decoder.damaged()),
        };
        log::debug!(target: "eagle_engine", consumed_frames=stats.consumed_frames, underruns=stats.underruns, callbacks=stats.callbacks, damaged_packets=stats.damaged_packets; "audio statistics");
        self.emit(Event::Statistics(stats));
    }
}
/// Compatibility entry point used by existing provider playback.
pub fn play(path: &Path) -> Result<(), Error> {
    let engine = Engine::new(Config::default())?;
    engine.enqueue(Source::file(path))?;
    let mut error = None;
    while !engine.finished() {
        if let Ok(Event::Error { message, .. }) = engine.recv_timeout(Duration::from_millis(100)) {
            error = Some(message);
        }
    }
    while let Ok(event) = engine.try_recv() {
        if let Event::Error { message, .. } = event {
            error = Some(message);
        }
    }
    if let Some(error) = error {
        return Err(Error::Decode(error));
    }
    Ok(())
}

#[derive(Debug)]
pub struct Inspection {
    pub input_rate: u32,
    pub output_rate: u32,
    pub frames: u64,
    pub damaged_packets: u64,
}
/// Decode to a counter without an audio device, retaining only a packet of PCM.
pub fn inspect(source: &Source, output_rate: Option<u32>) -> Result<Inspection, Error> {
    let reader = source
        .open(Interrupt {
            generation: Arc::new(AtomicU32::new(0)),
            expected: 0,
            preload: false,
            seeking: Arc::new(AtomicBool::new(false)),
        })
        .map_err(|e| Error::Decode(e.to_string()))?;
    let decoder = Decoder::open(reader)?;
    let input_rate = decoder.rate();
    let output_rate = output_rate.unwrap_or(input_rate);
    let mut pipeline = Pipeline::new(decoder, output_rate, Duration::ZERO)?;
    let mut frames = 0;
    loop {
        pipeline.fill()?;
        frames += pipeline.pending.len() as u64;
        pipeline.pending.clear();
        if pipeline.finished {
            if let Some(converter) = &mut pipeline.converter {
                frames += converter.finish()?.len() as u64;
            }
            break;
        }
    }
    Ok(Inspection {
        input_rate,
        output_rate,
        frames,
        damaged_packets: pipeline.decoder.damaged(),
    })
}
/// Provider metadata queue retained for clients that filter unavailable tracks.
#[derive(Default)]
pub struct Queue {
    tracks: VecDeque<eagle_core::Track>,
}
impl Queue {
    pub fn push(&mut self, track: eagle_core::Track) {
        self.tracks.push_back(track);
    }
    pub fn next_available(&mut self) -> Option<eagle_core::Track> {
        while let Some(track) = self.tracks.pop_front() {
            if track.availability == eagle_core::Availability::Available {
                return Some(track);
            }
        }
        None
    }
}
