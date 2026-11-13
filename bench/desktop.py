# SPDX-License-Identifier: GPL-3.0-or-later
"""Measure the real macOS desktop with an isolated, credential-free fixture."""

import argparse
import hashlib
import json
import os
import platform
import subprocess
import time
from pathlib import Path

from measure import mac_sample, wakeup_deltas


def percentile(values, fraction):
    """Nearest-rank percentile, including the final observation at p100."""
    import math
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def summarize(series):
    stages = {}
    for row in series:
        stages.setdefault(row["stage"], []).append(row)
    return {stage: {
        "samples": len(rows),
        "sampled_peak_bytes": max(r["phys_footprint_bytes"] for r in rows),
        "p95_bytes": percentile([r["phys_footprint_bytes"] for r in rows], .95),
        "settled_final_bytes": rows[-1]["phys_footprint_bytes"],
        "settled_last_second_p95_bytes": percentile([
            r["phys_footprint_bytes"] for r in rows if r["seconds"] >= rows[-1]["seconds"] - 1
        ], .95),
    } for stage, rows in stages.items()}


def prepare_artwork(root, mode):
    """Prepare bounded PNGs and stable cache filenames before the observed process."""
    import struct
    import zlib
    covers = root / 'covers'
    covers.mkdir(exist_ok=True)
    source = root / '.fixture-artwork'
    source.mkdir(exist_ok=True)
    count = 20 if mode == 'repeated' else 1000
    def chunk(kind, data):
        return (struct.pack('!I', len(data)) + kind + data
                + struct.pack('!I', zlib.crc32(kind + data)))
    for n in range(count):
        for edge in (64, 600):
            rgb = bytes((40 + n * 37 % 180, 40 + n * 71 % 180, 40 + n * 13 % 180))
            # Different stripe position also distinguishes images with the same palette.
            raw = b''.join(b'\0' + ((rgb if (y + n) % 31 > 4 else b'\xe0\xd8\xb0') * edge)
                           for y in range(edge))
            png = (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', edge, edge, 8, 2, 0, 0, 0))
                   + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
            (source / f'{n}-{edge}.png').write_bytes(png)
    for n in range(1, 10001):
        for edge in (64, 600):
            target = covers / f'fixture_{n}-{edge}.png'
            if not target.exists():
                os.link(source / f'{n % count}-{edge}.png', target)
    for n in range(1, 1001):
        for edge in (64, 600):
            target = covers / f'album_fixture_album{n}-{edge}.png'
            if not target.exists():
                os.link(source / f'{n % count}-{edge}.png', target)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--fixture-root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--software', action='store_true')
    parser.add_argument('--renderer', choices=['skia', 'femtovg-wgpu', 'software'], default=None)
    parser.add_argument('--reuse-fixture', action='store_true')
    parser.add_argument('--artwork', choices=['repeated', 'unique'], default='unique')
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--prepare-timeout', type=float, default=600)
    args = parser.parse_args()
    if args.runs < 1 or args.timeout <= 0:
        parser.error('runs and timeout must be positive')
    renderer = args.renderer or ('software' if args.software else 'femtovg-wgpu')
    if args.software and renderer != 'software':
        parser.error('conflicting renderer flags')
    if args.runs > 1:
        if args.reuse_fixture:
            parser.error('--reuse-fixture requires --runs 1; preserve each independent run separately')
        for run in range(1, args.runs + 1):
            command = [os.sys.executable, __file__, '--binary', str(args.binary),
                       '--fixture-root', str(args.fixture_root.with_name(args.fixture_root.name + f'-{run}')),
                       '--output', str(args.output.with_name(args.output.stem + f'-{run}' + args.output.suffix)),
                       '--runs', '1', '--renderer', renderer, '--timeout', str(args.timeout),
                       '--artwork', args.artwork, '--prepare-timeout', str(args.prepare_timeout)]
            subprocess.run(command, check=True)
        return
    if platform.system() != 'Darwin':
        parser.error('This sampler measures macOS physical footprint only')
    if args.fixture_root.exists() and not args.reuse_fixture:
        parser.error('Use a new fixture directory for each run, or --reuse-fixture for a warm run')
    if args.reuse_fixture and not all((args.fixture_root / name).is_file() for name in ('.eagle-fixture', '.fixture-prepared')):
        parser.error('--reuse-fixture requires a fully prepared, marked fixture')
    binary = args.binary.resolve(strict=True)
    root = args.fixture_root.resolve()
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([str(binary), '--fixture-root', str(root), '--fixture-prepare-only'],
                   env=dict(os.environ, EAGLE_DATA_DIR=str(root.with_name(root.name + '-runtime') / 'data')), check=True, timeout=args.prepare_timeout)
    prepare_artwork(root, args.artwork)
    command = [str(binary), '--fixture-root', str(root), '--fixture-smoke']
    if renderer == 'software':
        command.append('--software')
    elif renderer != 'skia':
        command.append('--renderer=' + renderer)
    result = {
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'os': platform.platform(),
        'machine': subprocess.check_output(['sysctl', '-n', 'hw.model'], text=True).strip(),
        'physical_memory_bytes': int(subprocess.check_output(
            ['sysctl', '-n', 'hw.memsize'], text=True)),
        'renderer_requested': renderer,
        'artwork': args.artwork,
        'disk_cache': 'prepared before launch; OS cache temperature is uncontrolled',
        'decoded_cache': 'cold at process start',
        'build_requirement': 'release binary without validation/debug GPU layers',
        'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], text=True)),
        'rust': subprocess.check_output(['rustc', '--version'], text=True).strip(),
        'slint': '1.17.0',
        'units': 'bytes; 1 MB = 1,000,000 bytes',
        'sample_interval_ms_requested': 10,
        'lifetime_peak_physical_footprint_bytes': None,
        'peak_limitation': 'sampled peak only; shorter spikes may be missed',
        'frame_presentation_intervals': 'not measured; requires separate presentation trace',
        'render_duration': 'not measured; requires separate GPU trace',
        'surface_physical_pixels': None,
        'scale_factor': None,
        'actual_graphics_api': 'software' if renderer == 'software' else 'unverified',
        'foreground_and_occlusion': 'unverified',
        'audible_output': 'not independently verified by sampler',
        'measurement': 'physical footprint; process wakeups do not separate UI and audio',
        'plateau_tail_seconds': 1,
        'valid_smoke': False,
        'samples': [],
    }
    # Logs stay with this measurement, outside both normal data and movable storage.
    env = dict(os.environ, EAGLE_DATA_DIR=str(root.with_name(root.name + '-runtime') / 'data'))
    with output.with_suffix('.process.log').open('w') as log:
        child = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        started = time.monotonic()
        series = []
        previous = None
        stage_start = started
        stage_first = None
        stage_last = None
        lifetime_peak = None
        try:
            deadline = started
            while child.poll() is None and time.monotonic() - started < args.timeout:
                marker = root / 'smoke-stage'
                stage = marker.read_text().strip() if marker.exists() else 'startup'
                now = time.monotonic()
                try:
                    sample = mac_sample(child.pid)
                except OSError:
                    if child.poll() is not None:
                        break
                    raise
                peak = sample[3].get('lifetime_max_phys_footprint_bytes')
                if peak is not None:
                    lifetime_peak = max(lifetime_peak or 0, peak)
                series.append({'seconds': now - started, 'stage': stage,
                               'phys_footprint_bytes': sample[1]})
                if previous != stage:
                    if stage_first is not None and stage_last is not None:
                        elapsed = now - stage_start
                        wakes = wakeup_deltas(stage_first, stage_last)
                        result['samples'].append({
                            'stage': previous, 'elapsed': elapsed,
                            'cpu_percent_one_core': (stage_last[0] - stage_first[0]) / elapsed * 100,
                            'process_package_idle_wakeups': wakes[0],
                            'process_interrupt_wakeups': wakes[1],
                        })
                    previous, stage_start, stage_first = stage, now, sample
                stage_last = sample
                deadline += .010
                time.sleep(max(0, deadline - time.monotonic()))
            if child.poll() is None:
                raise TimeoutError('Desktop smoke timed out')
            result['exit_code'] = child.returncode
            if child.returncode:
                raise RuntimeError('Desktop smoke failed; inspect the process log')
            expected = {'idle', 'playing', 'closed-playing', 'reopened-playing', 'paused',
                        'closed', 'reopened', 'closed-again', 'reopened-again'}
            if not expected.issubset({row['stage'] for row in series}):
                raise RuntimeError('Desktop smoke did not yield all required samples')
            result['valid_smoke'] = True
        except BaseException as error:
            result['error'] = str(error)
            raise
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            result['exit_code'] = child.returncode
            for metadata_file in ('fixture-gpu.json', 'fixture-window.json'):
                path = root / metadata_file
                if path.exists():
                    result.update(json.loads(path.read_text()))
            result['lifetime_peak_physical_footprint_bytes'] = lifetime_peak
            if lifetime_peak is not None:
                result['peak_limitation'] = 'system lifetime peak through final live sample; teardown after it is not observed'
            result['series'] = series
            result['stages'] = summarize(series)
            if series:
                result['sampled_peak_bytes'] = max(r['phys_footprint_bytes'] for r in series)
                result['p95_bytes'] = percentile([r['phys_footprint_bytes'] for r in series], .95)
                gaps = [b['seconds'] - a['seconds'] for a, b in zip(series, series[1:])]
                result['sample_gap_ms_max'] = max(gaps, default=0) * 1000
            output.write_text(json.dumps(result, indent=2) + '\n')
    print(output)


if __name__ == '__main__':
    main()
