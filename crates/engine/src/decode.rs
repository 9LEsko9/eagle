// SPDX-License-Identifier: GPL-3.0-or-later
use crate::{Error, source::Reader};
use std::io::{Read, Seek, SeekFrom};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{
        CODEC_TYPE_AAC, CODEC_TYPE_MP3, CODEC_TYPE_NULL, Decoder as SymphoniaDecoder,
        DecoderOptions,
    },
    errors::Error as DecodeError,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
    units::Time,
};

pub(crate) struct Chunk {
    pub frames: Vec<[f32; 2]>,
}
pub(crate) enum Decoder {
    Symphonia(Symphonia),
    Opus(Opus),
}
pub(crate) struct Symphonia {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaDecoder>,
    track: u32,
    rate: u32,
    channels: usize,
    duration: Option<f64>,
    seek_to: Option<u64>,
    damaged: u64,
    trim: Option<crate::mp4::Trim>,
    limit: Option<u64>,
    emitted: u64,
    trust_frame_count: bool,
    time_base: Option<symphonia::core::units::TimeBase>,
}
pub(crate) struct Opus {
    packets: ogg::reading::PacketReader<Reader>,
    decoder: opus::Decoder,
    channels: usize,
    pre_skip: u64,
    decoded: u64,
    gain: f32,
    serial: u32,
    damaged: u64,
}

impl Decoder {
    pub fn open(mut source: Reader) -> Result<Self, Error> {
        let mut head = [0; 64];
        let mut count = 0;
        while count < head.len() {
            let n = source.read(&mut head[count..])?;
            if n == 0 {
                break;
            }
            count += n;
        }
        source.seek(SeekFrom::Start(0))?;
        if head.starts_with(b"#EXTM3U") {
            return Err(Error::Unsupported("HLS не поддерживается"));
        }
        if head[..count].windows(8).any(|s| s == b"OpusHead") {
            return Ok(Self::Opus(Opus::open(source)?));
        }
        let trim = if head.get(4..8) == Some(b"ftyp") {
            crate::mp4::read(&mut source, 0)?
        } else {
            None
        };
        let stream = MediaSourceStream::new(source, Default::default());
        let mut format = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                stream,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )?
            .format;
        // Without LAME trim, the MP3 demuxer's duration may only be a bitrate
        // estimate. Do not let that estimate truncate PCM in gapless mode.
        let estimated_mp3 = format.tracks().iter().any(|t| {
            t.codec_params.codec == CODEC_TYPE_MP3
                && t.codec_params.delay.is_none()
                && t.codec_params.padding.is_none()
        });
        if estimated_mp3 {
            let mut stream = format.into_inner();
            stream.seek(SeekFrom::Start(0))?;
            format = symphonia::default::get_probe()
                .format(
                    &Hint::new(),
                    stream,
                    &FormatOptions::default(),
                    &MetadataOptions::default(),
                )?
                .format;
        }
        let track = format
            .tracks()
            .iter()
            .find(|t| {
                t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some()
            })
            .ok_or(Error::Unsupported("аудиодорожка не найдена"))?;
        let params = &track.codec_params;
        let rate = params
            .sample_rate
            .ok_or(Error::Unsupported("неизвестная частота"))?;
        if rate == 0 {
            return Err(Error::Unsupported("частота аудио равна нулю"));
        }
        let channels = params.channels.map_or(0, |c| c.count());
        if channels != 0 {
            check_channels(channels)?;
        }
        let trim = trim
            .and_then(|t| {
                Some(crate::mp4::Trim {
                    start: if t.rate == 0 {
                        t.start
                    } else {
                        t.start.checked_mul(rate as u64)? / t.rate as u64
                    },
                    length: if t.rate == 0 {
                        t.length
                    } else {
                        t.length
                            .checked_mul(rate as u64)?
                            .checked_add(t.rate as u64 / 2)?
                            / t.rate as u64
                    },
                    rate,
                })
            })
            .filter(|t| t.start.checked_add(t.length).is_some());
        let duration = trim
            .map(|t| t.length as f64 / rate as f64)
            .or_else(|| params.n_frames.map(|n| n as f64 / rate as f64));
        let trust_frame_count =
            !estimated_mp3 && (params.codec != CODEC_TYPE_AAC || head.get(4..8) == Some(b"ftyp"));
        let limit = trim.map(|t| t.length).or(if trust_frame_count {
            params.n_frames
        } else {
            None
        });
        let time_base = params.time_base;
        let decoder = symphonia::default::get_codecs().make(params, &DecoderOptions::default())?;
        let track = track.id;
        Ok(Self::Symphonia(Symphonia {
            format,
            decoder,
            track,
            rate,
            channels,
            duration,
            seek_to: None,
            damaged: 0,
            trim,
            limit,
            emitted: 0,
            trust_frame_count,
            time_base,
        }))
    }
    pub fn rate(&self) -> u32 {
        match self {
            Self::Symphonia(d) => d.rate,
            Self::Opus(_) => 48_000,
        }
    }
    pub fn duration(&self) -> Option<f64> {
        match self {
            Self::Symphonia(d) => d.duration,
            Self::Opus(_) => None,
        }
    }
    pub fn damaged(&self) -> u64 {
        match self {
            Self::Symphonia(d) => d.damaged,
            Self::Opus(d) => d.damaged,
        }
    }
    /// An unsupported fast seek is handled by reopening and decoding from the start.
    pub fn seek(&mut self, seconds: f64) -> Result<(), Error> {
        match self {
            Self::Symphonia(d) => {
                let target = seconds + d.trim.map_or(0., |t| t.start as f64 / d.rate as f64);
                // MP3 bit reservoirs and AAC overlap require decoder preroll after reset.
                d.format.seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time: Time::from((target - 1.).max(0.)),
                        track_id: Some(d.track),
                    },
                )?;
                d.decoder.reset();
                d.seek_to = Some((target * d.rate as f64).round() as u64);
                d.emitted = (seconds * d.rate as f64).round() as u64;
                Ok(())
            }
            Self::Opus(_) => Err(Error::Unsupported("Opus: поиск декодированием от начала")),
        }
    }
    pub fn next(&mut self) -> Result<Option<Chunk>, Error> {
        match self {
            Self::Symphonia(d) => d.next(),
            Self::Opus(d) => d.next(),
        }
    }
}
fn check_channels(channels: usize) -> Result<(), Error> {
    if !(1..=2).contains(&channels) {
        return Err(Error::Unsupported(
            "поддерживаются только mono/stereo, многоканальный звук не поддерживается",
        ));
    }
    Ok(())
}
impl Symphonia {
    fn next(&mut self) -> Result<Option<Chunk>, Error> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(DecodeError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    if self.damaged == 0 && self.limit.is_some_and(|n| self.emitted < n) {
                        return Err(Error::Unsupported("трек оборван до объявленного конца"));
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track {
                continue;
            }
            if self.trust_frame_count && self.trim.is_none() {
                self.limit = self
                    .format
                    .tracks()
                    .iter()
                    .find(|t| t.id == self.track)
                    .and_then(|t| t.codec_params.n_frames);
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(DecodeError::DecodeError(_)) => {
                    self.damaged += 1;
                    if self.damaged > 100 {
                        return Err(Error::Unsupported("слишком много повреждённых пакетов"));
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if self.channels == 0 {
                self.channels = decoded.spec().channels.count();
                check_channels(self.channels)?;
            }
            if decoded.spec().rate != self.rate || decoded.spec().channels.count() != self.channels
            {
                return Err(Error::Unsupported(
                    "параметры аудио изменились внутри трека",
                ));
            }
            let mut samples = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            samples.copy_interleaved_ref(decoded);
            // MP3/Vorbis decoders already apply packet trim; never apply it a second time.
            let packet_start = self.time_base.map_or(packet.ts(), |base| {
                let time = base.calc_time(packet.ts());
                ((time.seconds as f64 + time.frac) * self.rate as f64).round() as u64
            });
            let skip = self
                .seek_to
                .map_or(0, |target| target.saturating_sub(packet_start) as usize)
                .max(
                    self.trim
                        .map_or(0, |t| t.start.saturating_sub(packet_start) as usize),
                );
            let take = self.trim.map_or(usize::MAX, |t| {
                (t.start + t.length).saturating_sub(packet_start) as usize
            });
            if take == 0 {
                return Ok(None);
            }
            let remaining = self
                .limit
                .map_or(usize::MAX, |n| n.saturating_sub(self.emitted) as usize);
            if remaining == 0 {
                return Ok(None);
            }
            let frames: Vec<_> = samples
                .samples()
                .chunks_exact(self.channels)
                .take(take)
                .skip(skip)
                .take(remaining)
                .map(|s| [s[0], s[self.channels - 1]])
                .collect();
            if frames.is_empty() {
                continue;
            }
            self.seek_to = None;
            self.emitted += frames.len() as u64;
            return Ok(Some(Chunk { frames }));
        }
    }
}
impl Opus {
    fn open(source: Reader) -> Result<Self, Error> {
        let mut packets = ogg::reading::PacketReader::new(source);
        let head = packets
            .read_packet_expected()
            .map_err(|e| Error::Decode(e.to_string()))?;
        let h = &head.data;
        if h.len() < 19 || &h[..8] != b"OpusHead" || h[8] > 15 || h[18] != 0 {
            return Err(Error::Unsupported(
                "неподдерживаемый OpusHead/channel mapping",
            ));
        }
        let channels = h[9] as usize;
        check_channels(channels)?;
        let pre_skip = u16::from_le_bytes([h[10], h[11]]) as u64;
        let gain = 10f32.powf(i16::from_le_bytes([h[16], h[17]]) as f32 / (256.0 * 20.0));
        let decoder = opus::Decoder::new(
            48_000,
            if channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            },
        )
        .map_err(|e| Error::Decode(e.to_string()))?;
        let serial = head.stream_serial();
        let tags = packets
            .read_packet_expected()
            .map_err(|e| Error::Decode(e.to_string()))?;
        if !tags.data.starts_with(b"OpusTags") {
            return Err(Error::Unsupported("нет OpusTags"));
        }
        Ok(Self {
            packets,
            decoder,
            channels,
            pre_skip,
            decoded: 0,
            gain,
            serial,
            damaged: 0,
        })
    }
    fn next(&mut self) -> Result<Option<Chunk>, Error> {
        loop {
            let Some(packet) = self
                .packets
                .read_packet()
                .map_err(|e| Error::Decode(e.to_string()))?
            else {
                return Ok(None);
            };
            if packet.stream_serial() != self.serial {
                return Err(Error::Unsupported(
                    "цепочки Ogg Opus пока не поддерживаются",
                ));
            }
            let mut samples = vec![0f32; 5760 * self.channels];
            let count = match self.decoder.decode_float(&packet.data, &mut samples, false) {
                Ok(n) => n,
                Err(_) => {
                    self.damaged += 1;
                    if self.damaged > 100 {
                        return Err(Error::Unsupported("повреждённый Opus"));
                    }
                    continue;
                }
            };
            let start = self.decoded;
            self.decoded += count as u64;
            let end = if packet.last_in_stream() {
                packet.absgp_page().min(self.decoded)
            } else {
                self.decoded
            };
            let skip = self.pre_skip.saturating_sub(start).min(count as u64) as usize;
            let keep = end.saturating_sub(start).min(count as u64) as usize;
            let frames = samples[..keep * self.channels]
                .chunks_exact(self.channels)
                .skip(skip)
                .map(|s| [s[0] * self.gain, s[self.channels - 1] * self.gain])
                .collect::<Vec<_>>();
            if !frames.is_empty() {
                return Ok(Some(Chunk { frames }));
            }
        }
    }
}
