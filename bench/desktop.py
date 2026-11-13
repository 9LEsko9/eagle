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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--fixture-root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--software', action='store_true')
    args = parser.parse_args()
    if platform.system() != 'Darwin':
        parser.error('This sampler measures macOS physical footprint only')
    if args.fixture_root.exists():
        parser.error('Use a new fixture directory for each run')
    binary = args.binary.resolve(strict=True)
    root = args.fixture_root.resolve()
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    command = [str(binary), '--fixture-root', str(root), '--fixture-smoke']
    if args.software:
        command.append('--software')
    result = {
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'os': platform.platform(),
        'machine': subprocess.check_output(['sysctl', '-n', 'hw.model'], text=True).strip(),
        'physical_memory_bytes': int(subprocess.check_output(
            ['sysctl', '-n', 'hw.memsize'], text=True)),
        'renderer_requested': 'software' if args.software else 'skia',
        'actual_graphics_api': 'software' if args.software else 'unverified',
        'foreground_and_occlusion': 'unverified',
        'audible_output': 'not independently verified by sampler',
        'measurement': 'physical footprint; process wakeups do not separate UI and audio',
        'settle_seconds': 2,
        'sample_seconds': 2,
        'samples': [],
    }
    # Logs stay with this measurement, outside both normal data and movable storage.
    env = dict(os.environ, EAGLE_DATA_DIR=str(output.with_suffix('.data')))
    with output.with_suffix('.process.log').open('w') as log:
        child = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        started = time.monotonic()
        observed = set()
        try:
            while child.poll() is None and time.monotonic() - started < 120:
                marker = root / 'smoke-stage'
                stage = marker.read_text().strip() if marker.exists() else 'startup'
                if stage not in observed and stage not in ('startup', 'quit'):
                    observed.add(stage)
                    time.sleep(2)
                    if child.poll() is not None:
                        break
                    before = mac_sample(child.pid)
                    t0 = time.monotonic()
                    time.sleep(2)
                    if child.poll() is not None:
                        break
                    after = mac_sample(child.pid)
                    elapsed = time.monotonic() - t0
                    if marker.read_text().strip() != stage:
                        raise RuntimeError('Fixture changed stage during the sample')
                    wakes = wakeup_deltas(before, after)
                    result['samples'].append({
                        'stage': stage,
                        'elapsed': elapsed,
                        'cpu_percent_one_core': (after[0] - before[0]) / elapsed * 100,
                        'phys_footprint_bytes': after[1],
                        'process_package_idle_wakeups': wakes[0],
                        'process_interrupt_wakeups': wakes[1],
                    })
                    output.write_text(json.dumps(result, indent=2) + '\n')
                time.sleep(.1)
            if child.poll() is None:
                raise TimeoutError('Desktop smoke did not exit within 120 seconds')
            result['exit_code'] = child.returncode
            if child.returncode:
                raise RuntimeError('Desktop smoke failed; inspect the process log')
            expected = {'idle', 'playing', 'closed-playing', 'reopened-playing', 'paused',
                        'closed', 'reopened', 'closed-again', 'reopened-again'}
            if {s['stage'] for s in result['samples']} != expected:
                raise RuntimeError('Desktop smoke did not yield all required samples')
        except BaseException as error:
            result['error'] = str(error)
            raise
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            output.write_text(json.dumps(result, indent=2) + '\n')
    print(output)


if __name__ == '__main__':
    main()
