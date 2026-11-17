# SPDX-License-Identifier: GPL-3.0-or-later
"""Measure the real macOS desktop with an isolated, credential-free fixture."""

import argparse
import hashlib
import json
import os
import platform
import plistlib
import re
import shutil
import signal
import sqlite3
import subprocess
import time
from contextlib import closing
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


def completed_peak(path, observed):
    """Validate the process's final kernel high-water counter against live samples."""
    peak = int(path.read_text().strip())
    if peak <= 0 or peak < (observed or 0):
        raise ValueError('Final lifetime peak is missing or lower than a live observation')
    return peak


def observed_gpu_api(process_log):
    backends = set(re.findall(r'femtovg device backend=(\w+)', process_log))
    if backends != {'Metal'}:
        raise ValueError(f'Metal rendering was not observed consistently: {sorted(backends)}')
    return 'WGPU/Metal'


def scroll_metrics(process_log):
    """Slint render counts during the opt-in scroll stage, not presentation intervals."""
    stage = None
    samples = []
    for line in process_log.splitlines():
        if line.startswith('Eagle fixture stage: '):
            stage = line.removeprefix('Eagle fixture stage: ').strip()
        match = re.search(r'average frames per second: (\d+)', line)
        if stage == 'scrolling' and match:
            layers = re.search(r'\[(\d+) layers created\]', line)
            samples.append({'rendered_frames': int(match[1]),
                            'layers_created_last_frame': int(layers[1]) if layers else None})
    return samples


def app_command(binary, output, env, arguments):
    """Launch the measured desktop as a high-resolution macOS app, like an installed build."""
    bundle = output.with_suffix('.app')
    executable = bundle / 'Contents' / 'MacOS' / 'eagle-desktop'
    if app_pid(executable) is not None:
        raise RuntimeError('This benchmark app is already running')
    executable.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, executable)
    identifier = hashlib.sha256(os.fsencode(bundle)).hexdigest()[:16]
    with (bundle / 'Contents' / 'Info.plist').open('wb') as file:
        plistlib.dump(dict(CFBundleIdentifier='app.eagle.benchmark.' + identifier,
                          CFBundleName='Eagle Benchmark', CFBundleExecutable=executable.name,
                          CFBundlePackageType='APPL', CFBundleVersion='0.1.0',
                          NSHighResolutionCapable=True, NSPrincipalClass='NSApplication',
                          LSMinimumSystemVersion='11.0'), file)
    log = str(output.with_suffix('.process.log'))
    command = ['open', '-W', '-n', '--stdout', log, '--stderr', log]
    for key, value in env.items():
        if key in ('EAGLE_DATA_DIR', 'EAGLE_PROFILE_FRAME_TIME', 'EAGLE_FIXTURE_SCROLL_STEP', 'EAGLE_FIXTURE_SCROLL_OFFSCREEN', 'SLINT_DEBUG_PERFORMANCE') or key.startswith('Malloc'):
            command.extend(['--env', key + '=' + value])
    return [*command, str(bundle), '--args', *arguments], executable


def app_pid(executable):
    pattern = '^' + re.escape(str(executable)) + '( |$)'
    result = subprocess.run(['pgrep', '-f', pattern], capture_output=True, text=True)
    if result.returncode == 1:
        return None
    if result.returncode != 0 or len(result.stdout.split()) != 1:
        raise RuntimeError('Cannot identify exactly one benchmark app process')
    return int(result.stdout.strip())


def terminate_app(pid, executable):
    # A recycled PID or another Eagle instance must never be signalled.
    if pid is not None and app_pid(executable) == pid:
        os.kill(pid, signal.SIGTERM)


def fixture_queue_entries(root):
    """A reused fixture must still carry the full queue, likes and saved workload."""
    with closing(sqlite3.connect((root / 'metadata.sqlite').as_uri() + '?mode=ro', uri=True)) as db:
        states = db.execute('SELECT json FROM playback').fetchall()
        likes = db.execute('SELECT COUNT(*) FROM liked').fetchone()[0]
        offline = db.execute("SELECT COUNT(DISTINCT track) FROM downloads WHERE state='saved'").fetchone()[0]
        owners = db.execute('SELECT COUNT(DISTINCT track) FROM offline_owners').fetchone()[0]
    if len(states) != 1:
        raise ValueError('Fixture must contain exactly one playback state')
    state = json.loads(states[0][0])
    if len(state['entries']) != 10_000 or len(state['order']) != 10_000:
        raise ValueError('Fixture queue must contain 10,000 entries; prepare a fresh fixture')
    if likes != 10_000 or offline != 1_000 or owners != 1_000:
        raise ValueError('Fixture must contain 10,000 likes and 1,000 saved, owned tracks')
    return len(state['entries'])


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
    parser.add_argument('--scroll', action='store_true', help='Add a deterministic track-list scroll stage')
    parser.add_argument('--scroll-offscreen', action='store_true', help='Render scroll frames through GPU snapshots (implies --scroll)')
    parser.add_argument('--scroll-step', type=int, default=512, help='Logical pixels per 16 ms scroll tick')
    parser.add_argument('--artwork', choices=['repeated', 'unique'], default='unique')
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--prepare-timeout', type=float, default=600)
    parser.add_argument('--max-peak-mb', type=float,
                        help='Fail above this whole-process lifetime peak (decimal MB)')
    parser.add_argument('--window-size', nargs=2, type=int, metavar=('WIDTH', 'HEIGHT'),
                        help='Fixture client size in logical pixels')
    parser.add_argument('--window-position', nargs=2, type=int, metavar=('X', 'Y'),
                        help='Fixture window position in physical pixels')
    parser.add_argument('--launch-mode', choices=('app', 'direct'), default='app',
                        help='Launch Services app bundle (default), or direct CLI diagnostic')
    args = parser.parse_args()
    if args.scroll_offscreen:
        args.scroll = True
    if not 1 <= args.scroll_step <= 65535:
        parser.error('scroll-step must be in 1..65535')
    if args.runs < 1 or args.timeout <= 0:
        parser.error('runs and timeout must be positive')
    if args.window_size is not None and not (800 <= args.window_size[0] <= 7680
                                            and 480 <= args.window_size[1] <= 4320):
        parser.error('window size must fit desktop limits: 800..7680 by 480..4320')
    if args.max_peak_mb is not None:
        import math
        if not math.isfinite(args.max_peak_mb) or args.max_peak_mb <= 0:
            parser.error('max-peak-mb must be finite and positive')
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
                       '--launch-mode', args.launch_mode,
                       '--artwork', args.artwork, '--prepare-timeout', str(args.prepare_timeout)]
            if args.max_peak_mb is not None:
                command.extend(['--max-peak-mb', str(args.max_peak_mb)])
            if args.scroll:
                command.extend(['--scroll', '--scroll-step', str(args.scroll_step)])
            if args.scroll_offscreen:
                command.append('--scroll-offscreen')
            for option, values in (('--window-size', args.window_size),
                                   ('--window-position', args.window_position)):
                if values is not None:
                    command.extend([option, *map(str, values)])
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
    queue_entries = fixture_queue_entries(root)
    if args.window_size is not None or args.window_position is not None:
        preferences_path = root / 'desktop.json'
        preferences = json.loads(preferences_path.read_text()) if preferences_path.exists() else {}
        if args.window_size is not None:
            preferences.update(zip(('width', 'height'), args.window_size))
        if args.window_position is not None:
            preferences.update(zip(('x', 'y'), args.window_position))
        preferences_path.write_text(json.dumps(preferences))
    # A warm fixture must never supply another process's markers or peak.
    for name in ('smoke-stage', 'fixture-peak.bytes', 'fixture-gpu.json', 'fixture-window.json',
                 'fixture-allocator.env'):
        (root / name).unlink(missing_ok=True)
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
        'launch_mode': args.launch_mode,
        'exit_status_limitation': ('Launch Services reports launcher status; the app is validated by its final peak file'
                                   if args.launch_mode == 'app' else None),
        'window_size_requested_logical': args.window_size,
        'window_position_requested_physical': args.window_position,
        'queue_entries_before': queue_entries,
        'fixture_likes': 10_000,
        'fixture_offline_tracks': 1_000,
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
        'windows_by_stage': {},
        'scroll_max_first_row': 0,
        'scroll_step_pixels': args.scroll_step if args.scroll else None,
        'scroll_render_mode': 'offscreen-readback' if args.scroll_offscreen else 'window',
        'samples': [],
    }
    # Logs stay with this measurement, outside both normal data and movable storage.
    env = dict(os.environ, EAGLE_DATA_DIR=str(root.with_name(root.name + '-runtime') / 'data'))
    if args.scroll:
        command.append('--fixture-scroll-smoke')
        env['SLINT_DEBUG_PERFORMANCE'] = 'refresh_lazy,console'
        env['EAGLE_FIXTURE_SCROLL_STEP'] = str(args.scroll_step)
        if args.scroll_offscreen:
            env['EAGLE_FIXTURE_SCROLL_OFFSCREEN'] = '1'
    app_executable = None
    if args.launch_mode == 'app':
        command, app_executable = app_command(binary, output, env, command[1:])
    with output.with_suffix('.process.log').open('w') as log:
        child = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        started = time.monotonic()
        series = []
        previous = None
        stage_start = started
        stage_first = None
        stage_last = None
        lifetime_peak = None
        measured_pid = child.pid if app_executable is None else None
        try:
            if app_executable is not None:
                while child.poll() is None and time.monotonic() - started < 15:
                    measured_pid = app_pid(app_executable)
                    if measured_pid is not None:
                        break
                    time.sleep(.05)
                if measured_pid is None:
                    raise RuntimeError('Launch Services did not start the benchmark app')
            result['measured_pid'] = measured_pid
            deadline = started
            while child.poll() is None and time.monotonic() - started < args.timeout:
                marker = root / 'smoke-stage'
                stage = marker.read_text().strip() if marker.exists() else 'startup'
                now = time.monotonic()
                try:
                    sample = mac_sample(measured_pid)
                except OSError:
                    if app_executable is not None:
                        child.wait(timeout=5)
                    if child.poll() is not None:
                        break
                    raise
                peak = sample[3].get('lifetime_max_phys_footprint_bytes')
                if peak is not None:
                    lifetime_peak = max(lifetime_peak or 0, peak)
                series.append({'seconds': now - started, 'stage': stage,
                               'phys_footprint_bytes': sample[1]})
                try:
                    window = json.loads((root / 'fixture-window.json').read_text())
                    if window.get('stage') == stage:
                        result['windows_by_stage'][stage] = window
                        if stage == 'scrolling' and window.get('visible_track_rows'):
                            result['scroll_max_first_row'] = max(
                                result['scroll_max_first_row'], window['visible_track_rows'][0])
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
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
            result['exit_code'] = child.returncode if app_executable is None else None
            result['launcher_exit_code'] = child.returncode
            if child.returncode:
                raise RuntimeError('Desktop smoke failed; inspect the process log')
            expected = {'idle', 'playing', 'closed-playing', 'reopened-playing', 'paused',
                        'closed', 'reopened', 'closed-again', 'reopened-again'}
            if not expected.issubset({row['stage'] for row in series}):
                raise RuntimeError('Desktop smoke did not yield all required samples')
            lifetime_peak = completed_peak(root / 'fixture-peak.bytes', lifetime_peak)
            result['queue_entries_after'] = fixture_queue_entries(root)
            if renderer == 'femtovg-wgpu':
                result['actual_graphics_api'] = observed_gpu_api(output.with_suffix('.process.log').read_text())
            if args.scroll:
                result['scroll_render_samples'] = scroll_metrics(output.with_suffix('.process.log').read_text())
                if sum(sample['rendered_frames'] > 0 for sample in result['scroll_render_samples']) < 2:
                    raise RuntimeError('Scroll stage did not yield enough render samples')
                if result['scroll_max_first_row'] == 0:
                    raise RuntimeError('Track-list viewport did not move during scroll stage')
            result['allocator_environment'] = dict(
                line.split('=', 1) for line in (root / 'fixture-allocator.env').read_text().splitlines()
            )
            for stage in ('idle', 'playing', 'reopened-playing', 'paused', 'reopened', 'reopened-again'):
                window = result['windows_by_stage'].get(stage, {})
                if window.get('queue_entries') != queue_entries:
                    raise RuntimeError(f'Full stress queue was not observed during {stage}')
                if not (window.get('window_open') and window.get('visible_to_user')
                        and window.get('native_window')):
                    raise RuntimeError(f'Visible native window was not observed during {stage}')
            for stage, status in (('playing', 'Playing'), ('reopened-playing', 'Playing'), ('paused', 'Paused')):
                if result['windows_by_stage'][stage].get('player_status') != status:
                    raise RuntimeError(f'Expected {status} player state during {stage}')
            result['peak_limitation'] = 'kernel lifetime peak recorded after UI teardown and logger flush; process exit follows'
            result['valid_smoke'] = True
            if args.max_peak_mb is not None:
                result['peak_budget_bytes'] = int(args.max_peak_mb * 1_000_000)
                result['peak_budget_passed'] = lifetime_peak <= result['peak_budget_bytes']
                if not result['peak_budget_passed']:
                    raise RuntimeError(f'Desktop lifetime peak {lifetime_peak} bytes exceeds '
                                       f'{result["peak_budget_bytes"]} bytes')
        except BaseException as error:
            result['error'] = str(error)
            raise
        finally:
            if child.poll() is None:
                if app_executable is not None:
                    terminate_app(measured_pid, app_executable)
                child.kill()
                child.wait()
            result['exit_code'] = child.returncode if app_executable is None else None
            result['launcher_exit_code'] = child.returncode
            for metadata_file in ('fixture-gpu.json', 'fixture-window.json'):
                path = root / metadata_file
                if path.exists():
                    result.update(json.loads(path.read_text()))
            result['lifetime_peak_physical_footprint_bytes'] = lifetime_peak
            if lifetime_peak is not None and not result['valid_smoke']:
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
