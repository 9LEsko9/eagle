# SPDX-FileCopyrightText: 2026 eagle contributors
# SPDX-License-Identifier: GPL-3.0-or-later

"""Idle baseline. Python 3.9+ is required on the measurement host."""
import argparse
import csv
import ctypes as C
import datetime
import os
from pathlib import Path
import platform
import subprocess
import time
import tempfile


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


def mac_sample(pid):
    # rusage_info_v2: UUID followed by 18 uint64 fields (Darwin sys/resource.h).
    class Usage(C.Structure):
        _fields_ = [('uuid', C.c_byte * 16), ('values', C.c_uint64 * 18)]
    lib = C.CDLL('/usr/lib/libproc.dylib', use_errno=True)
    usage = Usage()
    if lib.proc_pid_rusage(pid, 2, C.byref(usage)) != 0:
        raise OSError(C.get_errno(), 'proc_pid_rusage')
    return (usage.values[0] + usage.values[1]) / 1e9, usage.values[7], None


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--scenario', choices=['idle', 'info'], default='idle')
    args = parser.parse_args()
    if not 0 < args.seconds <= 86400:
        parser.error('--seconds must be in (0, 86400]')
    system = platform.system()
    sample, metric = {'Linux': (linux_sample, 'pss_bytes'),
                      'Darwin': (mac_sample, 'phys_footprint_bytes'),
                      'Windows': (windows_sample, 'private_working_set_bytes')}[system]
    subprocess.run(['cargo', 'build', '--release', '-p', 'eagle-cli'], check=True)
    binary = Path('target/release/eagle-cli' + ('.exe' if system == 'Windows' else '')).resolve()
    data = tempfile.TemporaryDirectory(prefix='eagle-bench-')
    env = dict(os.environ, EAGLE_DATA_DIR=data.name, EAGLE_LOG='info')
    child = subprocess.Popen([str(binary), '--bench-' + args.scenario], env=env, stderr=subprocess.DEVNULL)
    try:
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
            writer.writerow(['scenario', 'os', 'arch', 'duration_seconds', 'cpu_percent_one_core', metric, 'voluntary_switches_observed', 'binary_bytes', 'info_records_per_second'])
            writer.writerow([args.scenario, system, platform.machine(), elapsed,
                             (end[0] - start[0]) / elapsed * 100, end[1],
                             '' if end[2] is None else sum(row[3] for row in rows if row[3] != ''),
                             binary.stat().st_size, 1 if args.scenario == 'info' else 0])
        print(output)
    finally:
        child.terminate()
        child.wait(timeout=10)
        data.cleanup()


if __name__ == '__main__':
    main()
