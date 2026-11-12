# SPDX-License-Identifier: GPL-3.0-or-later
"""Regenerate our synthetic, redistributable codec fixtures with FFmpeg."""
import argparse
import math
from pathlib import Path
import struct
import subprocess
import wave


def wav(path, rate, channels, frames, offset=0):
    with wave.open(str(path), 'wb') as stream:
        stream.setparams((channels, 2, rate, frames, 'NONE', 'not compressed'))
        data = bytearray()
        for i in range(offset, offset + frames):
            value = int(10000 * (0.7 * math.sin(2 * math.pi * 440 * i / rate)
                                 + 0.3 * math.sin(2 * math.pi * 997 * i / rate)))
            data.extend(struct.pack('<h', value) * channels)
        stream.writeframes(data)


def long_vbr(directory, ffmpeg):
    # Alternating silence and deterministic chirps gives a non-constant bitrate.
    original = directory / 'long-vbr.wav'
    with wave.open(str(original), 'wb') as stream:
        stream.setparams((2, 2, 44100, 0, 'NONE', 'not compressed'))
        for second in range(8):
            data = bytearray()
            for i in range(44100):
                value = 0 if second % 2 == 0 else int(12000 * math.sin(
                    2 * math.pi * (220 * i / 44100 + 1500 * (i / 44100) ** 2)))
                data.extend(struct.pack('<hh', value, value))
            stream.writeframes(data)
    for name, options in [
            ('long-vbr.mp3', ['-c:a', 'libmp3lame', '-q:a', '2']),
            ('long-vbr-no-index.mp3', ['-c:a', 'libmp3lame', '-q:a', '2', '-write_xing', '0']),
            ('long-vbr.aac', ['-c:a', 'aac', '-q:a', '2'])]:
        subprocess.run([ffmpeg, '-v', 'error', '-y', '-i', str(original),
                        *options, str(directory / name)], check=True)
    original.unlink()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ffmpeg', default='ffmpeg')
    parser.add_argument('--directory', type=Path, default=Path('crates/engine/tests/fixtures'))
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    long_vbr(args.directory, args.ffmpeg)
    for rate in (44100, 48000):
        for channels in (1, 2):
            stem = args.directory / f'tone-{rate}-{channels}'
            original = stem.with_suffix('.wav')
            wav(original, rate, channels, rate // 4)
            for suffix, codec in [('mp3', 'libmp3lame'), ('aac', 'aac'), ('m4a', 'aac'),
                                  ('flac', 'flac'), ('ogg', 'libvorbis'), ('opus', 'libopus')]:
                subprocess.run([args.ffmpeg, '-v', 'error', '-y', '-i', str(original),
                                '-c:a', codec, str(stem.with_suffix('.' + suffix))], check=True)
    for i in range(2):
        wav(args.directory / f'split-{i}.wav', 44100, 2, 22050, i * 22050)
    wav(args.directory / 'continuous.wav', 44100, 2, 44100)
    wav(args.directory / 'multichannel.wav', 48000, 6, 96)
    for name in ('continuous', 'split-0', 'split-1'):
        subprocess.run([args.ffmpeg, '-v', 'error', '-y', '-i',
                        str(args.directory / (name + '.wav')), '-c:a', 'libmp3lame',
                        str(args.directory / (name + '.mp3'))], check=True)


if __name__ == '__main__':
    main()
