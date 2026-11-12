# SPDX-FileCopyrightText: 2026 eagle contributors
# SPDX-License-Identifier: GPL-3.0-or-later

"""Idle baseline. Python 3.9+ is required on the measurement host."""
import argparse
import csv
import ctypes as C
import datetime
import functools
import os
from pathlib import Path
import platform
import re
import subprocess
import tempfile
import threading
import time


def measure_library(binary, scenario, seconds, sample, metric):
    """Disk-local fixtures: cold decoded cache, repeated traversal, then plateau."""
    import json
    import struct
    import zlib
    with tempfile.TemporaryDirectory(prefix='eagle-library-') as directory:
        root = Path(directory)
        covers = root / 'covers'
        covers.mkdir()
        def chunk(kind, data):
            return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data))
        for edge in (64, 600):
            raw = (b'\0' + bytes([64, 128, 192, 255]) * edge) * edge
            png = (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', edge, edge, 8, 6, 0, 0, 0))
                   + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b''))
            (covers / f'fixture-{edge}.png').write_bytes(png)
        output = Path('bench/results') / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + f'.{scenario}.json')
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryFile(mode='w+') as progress:
            child = subprocess.Popen([str(binary), '--bench-library-' + scenario],
                                     env=dict(os.environ, EAGLE_DATA_DIR=directory), stdout=progress)
            snapshots = []
            started = time.monotonic()
            done_at = None
            pass_ready = {}
            pass_settled = {}
            try:
                while True:
                    if child.poll() is not None:
                        raise RuntimeError('Library fixture exited early')
                    snapshots.append((time.monotonic() - started, sample(child.pid)))
                    now = time.monotonic()
                    for index in range(3):
                        if (root / f'bench-pass-{index}').exists() and index not in pass_ready:
                            pass_ready[index] = now
                        if index in pass_ready and index not in pass_settled and now - pass_ready[index] >= 1:
                            pass_settled[index] = snapshots[-1][1][1]
                            (root / f'bench-ack-{index}').write_text('continue')
                    if (root / 'bench-done').exists() and done_at is None:
                        done_at = time.monotonic()
                    if done_at is not None and time.monotonic() - done_at >= seconds:
                        break
                    if time.monotonic() - started > 600:
                        raise RuntimeError('Library fixture timed out')
                    time.sleep(0.025)
            finally:
                child.terminate()
                child.wait(timeout=10)
            progress.seek(0)
            passes = [json.loads(line) for line in progress if line.strip()]
        for item in passes:
            item['settled_bytes'] = pass_settled[item['pass']]
        elapsed = snapshots[-1][0] - snapshots[0][0]
        settled = [s[1][1] for s in snapshots if s[0] >= done_at - started]
        report = dict(scenario=scenario, os=platform.system(), arch=platform.machine(),
                      binary=str(binary), duration_seconds=elapsed,
                      cpu_percent_one_core=(snapshots[-1][1][0] - snapshots[0][1][0]) / elapsed * 100,
                      memory_metric=metric, sampled_peak_bytes=max(s[1][1] for s in snapshots),
                      settled_min_bytes=min(settled), settled_max_bytes=max(settled),
                      settled_final_bytes=settled[-1], passes=passes,
                      samples=len(snapshots), sample_interval_ms=25)
        output.write_text(json.dumps(report, indent=2) + '\n')
        print(output)


def linux_sample(pid):
    base = Path('/proc') / str(pid)
    stat = (base / 'stat').read_text().rsplit(')', 1)[1].split()
    cpu = (int(stat[11]) + int(stat[12])) / os.sysconf('SC_CLK_TCK')
    pss = next(int(line.split()[1]) * 1024 for line in
               (base / 'smaps_rollup').read_text().splitlines() if line.startswith('Pss:'))
    return cpu, pss, linux_threads(base)


def linux_threads(base):
    threads = {}
    for task in (base / 'task').iterdir():
        try:
            status = (task / 'status').read_text().splitlines()
            name = (task / 'comm').read_text().strip()
            # Start time disambiguates reused TIDs.
            birth = (task / 'stat').read_text().rsplit(')', 1)[1].split()[19]
            count = next(int(line.split()[1]) for line in status
                         if line.startswith('voluntary_ctxt_switches:'))
            threads[(task.name, birth)] = (name, count)
        except FileNotFoundError:
            continue  # A thread may exit while /proc is being sampled.
    return threads


def thread_deltas(start, end):
    rows = []
    for key in sorted(start.keys() | end.keys()):
        if key not in end:
            rows.append((*key, start[key][0], '', 'exited'))
        elif key not in start:
            rows.append((*key, end[key][0], end[key][1], 'new_partial_window'))
        else:
            delta = end[key][1] - start[key][1]
            if delta < 0:
                raise ValueError('Thread switch counter decreased')
            rows.append((*key, end[key][0], delta, 'full_window'))
    return rows


@functools.lru_cache(maxsize=1)
def mac_seconds_per_tick():
    class Timebase(C.Structure):
        _fields_ = [('numer', C.c_uint32), ('denom', C.c_uint32)]
    timebase = Timebase()
    lib = C.CDLL('/usr/lib/libSystem.B.dylib')
    if lib.mach_timebase_info(C.byref(timebase)) != 0 or not timebase.denom:
        raise RuntimeError('mach_timebase_info failed')
    return timebase.numer / timebase.denom / 1e9


def mac_sample(pid):
    # rusage_info_v2: UUID followed by 18 uint64 fields (Darwin sys/resource.h).
    class Usage(C.Structure):
        _fields_ = [('uuid', C.c_byte * 16), ('values', C.c_uint64 * 18)]
    lib = C.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    usage = Usage()
    if lib.proc_pid_rusage(pid, 2, C.byref(usage)) != 0:
        raise OSError(C.get_errno(), 'proc_pid_rusage')
    return ((usage.values[0] + usage.values[1]) * mac_seconds_per_tick(), usage.values[7], None,
            {'package_idle_wakeups': usage.values[2], 'interrupt_wakeups': usage.values[3]})


def windows_sample(pid):
    from ctypes import wintypes as W
    kernel = C.WinDLL('kernel32', use_last_error=True)
    psapi = C.WinDLL('psapi', use_last_error=True)
    kernel.OpenProcess.restype = W.HANDLE
    kernel.OpenProcess.argtypes = [W.DWORD, W.BOOL, W.DWORD]
    kernel.CloseHandle.argtypes = [W.HANDLE]
    kernel.GetProcessTimes.argtypes = [W.HANDLE] + [C.POINTER(W.FILETIME)] * 4
    psapi.QueryWorkingSet.argtypes = [W.HANDLE, C.c_void_p, W.DWORD]
    handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        raise C.WinError(C.get_last_error())
    try:
        times = [W.FILETIME() for _ in range(4)]
        if not kernel.GetProcessTimes(handle, *(C.byref(t) for t in times)):
            raise C.WinError(C.get_last_error())
        cpu = sum((t.dwHighDateTime << 32) + t.dwLowDateTime for t in times[2:]) / 1e7
        # PSAPI_WORKING_SET_BLOCK.Shared is bit 8; entries represent pages.
        size = 65536
        while size <= 256 * 1024 * 1024:
            buf = C.create_string_buffer(size)
            if psapi.QueryWorkingSet(handle, buf, size):
                words = C.cast(buf, C.POINTER(C.c_size_t))
                count = words[0]
                private_pages = sum(not (words[i + 1] & (1 << 8)) for i in range(count))
                return cpu, private_pages * 4096, None
            if C.get_last_error() != 24:  # ERROR_BAD_LENGTH
                raise C.WinError(C.get_last_error())
            size *= 2
        raise RuntimeError('Working set exceeds sampler buffer limit')
    finally:
        kernel.CloseHandle(handle)


def wakeup_deltas(start, end):
    if len(start) < 4 or len(end) < 4:
        return ['', '']
    deltas = [end[3][key] - start[3][key]
              for key in ('package_idle_wakeups', 'interrupt_wakeups')]
    if any(value < 0 for value in deltas):
        raise ValueError('Process wakeup counters decreased')
    return deltas


def network_stats(output):
    match = re.search(r"Сеть: (\d+) запросов, (\d+) байт ответов", output)
    if not match:
        raise ValueError('CLI did not return network counters')
    return tuple(map(int, match.groups()))


def measure_track(binary, data_dir, track_id, sample, metric):
    if not re.fullmatch(r'yandex:[0-9]+', track_id):
        raise ValueError('--track must be yandex:<numeric ID>')
    env = dict(os.environ, EAGLE_DATA_DIR=data_dir, EAGLE_LOG='info')
    rows = []
    for mode in ('cold', 'cached'):
        snapshots = []
        sample_times = []
        started = time.monotonic()
        child = subprocess.Popen([str(binary), 'play', track_id, '--download-only'],
                                 env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True)
        stopped = threading.Event()

        def observe():
            while not stopped.is_set():
                try:
                    snapshots.append(sample(child.pid))
                    sample_times.append(time.monotonic())
                except (OSError, ValueError, ProcessLookupError):
                    pass  # Cached execution may finish between samples.
                stopped.wait(0.025)

        observer = threading.Thread(target=observe, daemon=True)
        observer.start()
        try:
            output, errors = child.communicate(timeout=300)
        except BaseException:
            child.kill()
            child.communicate()
            raise
        finally:
            stopped.set()
            observer.join()
        elapsed = time.monotonic() - started
        if child.returncode:
            raise RuntimeError('Track measurement failed: ' + errors.strip())
        requests, response_bytes = network_stats(output)
        if mode == 'cold' and requests == 0:
            raise RuntimeError('Cold run unexpectedly used a cache')
        if mode == 'cached' and (requests or response_bytes):
            raise RuntimeError('Cached run accessed the network')
        audio_bytes = sum(p.stat().st_size for p in (Path(data_dir) / 'audio').glob('*.high'))
        rows.append([mode, platform.system(), platform.machine(), track_id, elapsed,
                     requests, response_bytes, audio_bytes,
                     max((s[1] for s in snapshots), default=''), len(snapshots),
                     *(wakeup_deltas(snapshots[0], snapshots[-1]) if len(snapshots) >= 2 else ['', '']),
                     sample_times[-1] - sample_times[0] if len(sample_times) >= 2 else ''])
    output = Path('bench/results') / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '.track.csv')
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open('w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['mode', 'os', 'arch', 'track_id', 'duration_seconds',
                         'http_requests', 'response_body_bytes', 'audio_bytes',
                         'sampled_peak_' + metric, 'memory_samples',
                         'sampled_package_idle_wakeups', 'sampled_interrupt_wakeups', 'sampled_window_seconds'])
        writer.writerows(rows)
    print(output)


def measure_playback(binary, data_dir, track_id, seconds, sample, metric):
    if not re.fullmatch(r'yandex:[0-9]+', track_id):
        raise ValueError('--track must be yandex:<numeric ID>')
    cache = Path(data_dir) / 'audio' / ('yandex_' + track_id.split(':')[1] + '.high')
    import json
    ranges = json.loads(cache.with_suffix('.high.map').read_text())
    if (ranges['ranges'] != [[0, ranges['total']]]
            or cache.stat().st_size != ranges['total']):
        raise ValueError('Playback measurement requires a complete cached track')
    child = subprocess.Popen([str(binary), 'play', track_id],
                             env=dict(os.environ, EAGLE_DATA_DIR=str(data_dir), EAGLE_LOG='info'),
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        time.sleep(2)  # Exclude opening the DB, decoder and output device.
        if child.poll() is not None:
            raise RuntimeError('Playback exited during warmup')
        start = sample(child.pid)
        before = time.monotonic()
        time.sleep(seconds)
        end = sample(child.pid)
        elapsed = time.monotonic() - before
        if child.poll() is not None:
            raise RuntimeError('Track ended before the measurement finished; use fewer seconds')
    finally:
        child.terminate()
        output, errors = child.communicate(timeout=10)
    requests, response_bytes = network_stats(output)
    if requests or response_bytes:
        raise RuntimeError('Cached playback unexpectedly accessed the network')
    path = Path('bench/results') / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '.playback.csv')
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['scenario', 'os', 'arch', 'track_id', 'duration_seconds',
                         'cpu_percent_one_core', metric, 'http_requests', 'response_body_bytes',
                         'package_idle_wakeups', 'interrupt_wakeups'])
        writer.writerow(['playback', platform.system(), platform.machine(), track_id, elapsed,
                         (end[0] - start[0]) / elapsed * 100, end[1], requests, response_bytes,
                         *wakeup_deltas(start, end)])
    print(path)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--scenario', choices=['idle', 'info', 'store', 'track', 'playback', 'sync', 'scroll'], default='idle')
    parser.add_argument('--data-dir', type=Path, help='existing cache directory for playback measurement')
    parser.add_argument('--binary', type=Path, help='prebuilt CLI, e.g. a baseline build')
    parser.add_argument('--track', help='yandex:<track ID>; uses the existing keyring token')
    args = parser.parse_args()
    if args.scenario in ('track', 'playback') and not args.track:
        parser.error('this scenario requires --track yandex:<ID>')
    if args.scenario == 'playback' and not args.data_dir:
        parser.error('--scenario playback requires --data-dir with a complete cached track')
    if not 0 < args.seconds <= 86400:
        parser.error('--seconds must be in (0, 86400]')
    system = platform.system()
    sample, metric = {'Linux': (linux_sample, 'pss_bytes'),
                      'Darwin': (mac_sample, 'phys_footprint_bytes'),
                      'Windows': (windows_sample, 'private_working_set_bytes')}[system]
    if args.binary:
        binary = args.binary.resolve(strict=True)
    else:
        subprocess.run(['cargo', 'build', '--release', '--locked', '-p', 'eagle-cli'], check=True)
        binary = Path('target/release/eagle-cli' + ('.exe' if system == 'Windows' else '')).resolve()
    if args.scenario in ('sync', 'scroll'):
        measure_library(binary, args.scenario, args.seconds, sample, metric)
        return
    if args.scenario == 'playback':
        measure_playback(binary, args.data_dir.resolve(), args.track, args.seconds, sample, metric)
        return
    data = tempfile.TemporaryDirectory(prefix='eagle-bench-')
    if args.scenario == 'track':
        try:
            measure_track(binary, data.name, args.track, sample, metric)
        finally:
            data.cleanup()
        return
    env = dict(os.environ, EAGLE_DATA_DIR=data.name, EAGLE_LOG='info')
    child = subprocess.Popen([str(binary), '--bench-' + args.scenario], env=env, stderr=subprocess.DEVNULL)
    try:
        if args.scenario == 'store':
            deadline = time.monotonic() + 120
            while not (Path(data.name) / 'bench-ready').exists():
                if child.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError('Storage fixture did not become ready')
                time.sleep(0.025)
        time.sleep(1.25)  # Let startup info flush before measuring idle.
        start = sample(child.pid)
        before = time.monotonic()
        time.sleep(args.seconds)
        end = sample(child.pid)
        elapsed = time.monotonic() - before
        if child.poll() is not None:
            raise RuntimeError('Scenario exited before measurement finished')
        output = Path('bench/results') / (datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ') + '.csv')
        output.parent.mkdir(parents=True, exist_ok=True)
        rows = [] if end[2] is None else thread_deltas(start[2], end[2])
        if args.scenario == 'idle' and system == 'Linux':
            logger = [row for row in rows if row[2] == 'eg-log']
            if len(logger) != 1 or logger[0][3] != 0 or logger[0][4] != 'full_window':
                raise RuntimeError('Idle logger must have zero voluntary switches: ' + repr(logger))
        if end[2] is not None:
            with output.with_suffix('.threads.csv').open('w', newline='') as stream:
                writer = csv.writer(stream)
                writer.writerow(['tid', 'start_ticks', 'name', 'voluntary_switches', 'coverage'])
                writer.writerows(rows)
        with output.open('w', newline='') as stream:
            writer = csv.writer(stream)
            writer.writerow(['scenario', 'os', 'arch', 'duration_seconds', 'cpu_percent_one_core', metric, 'voluntary_switches_observed', 'binary_bytes', 'info_records_per_second', 'package_idle_wakeups', 'interrupt_wakeups'])
            writer.writerow([args.scenario, system, platform.machine(), elapsed,
                             (end[0] - start[0]) / elapsed * 100, end[1],
                             '' if end[2] is None else sum(row[3] for row in rows if row[3] != ''),
                             binary.stat().st_size, 1 if args.scenario == 'info' else 0,
                             *wakeup_deltas(start, end)])
        print(output)
    finally:
        child.terminate()
        child.wait(timeout=10)
        data.cleanup()


if __name__ == '__main__':
    main()
