# SPDX-FileCopyrightText: 2026 eagle contributors
# SPDX-License-Identifier: GPL-3.0-or-later

"""Build isolated release probes; flexi_logger never enters the workspace lockfile.

Usage: python3 bench/compare_loggers.py [--seconds 60]
Requires network access to Cargo's registry on the first run.
"""
import argparse
import csv
import datetime
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time

from measure import linux_sample, mac_sample, windows_sample, thread_deltas


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--seconds', type=float, default=60)
    args = parser.parse_args()
    if not 0 < args.seconds <= 86400:
        parser.error('--seconds must be in (0, 86400]')
    root = Path(__file__).resolve().parents[1]
    system = platform.system()
    sample, metric = {'Linux': (linux_sample, 'pss_bytes'),
                      'Darwin': (mac_sample, 'phys_footprint_bytes'),
                      'Windows': (windows_sample, 'private_working_set_bytes')}[system]
    own = '''let _logger = eagle_platform::logging::Logger::init(std::path::Path::new(&dir)).unwrap();'''
    flexi = '''let _logger = flexi_logger::Logger::try_with_str("info").unwrap()
        .log_to_file(flexi_logger::FileSpec::default().directory(&dir))
        .duplicate_to_stderr(flexi_logger::Duplicate::All)
        .rotate(flexi_logger::Criterion::Size(1_000_000), flexi_logger::Naming::Numbers,
            flexi_logger::Cleanup::KeepLogFiles(1))
        .write_mode(flexi_logger::WriteMode::BufferAndFlushWith(65536, std::time::Duration::from_secs(1)))
        .start().unwrap();'''
    log = 'log = { version = "0.4", features = ["std", "kv", "release_max_level_debug"] }\n'
    variants = [
        ('empty', '', ''),
        ('own', log + 'eagle-platform = { path = ' + json.dumps(str(root / 'crates/platform')) + ' }\n', own),
        ('flexi', log + 'flexi_logger = { version = "=0.31.10", default-features = false }\n', flexi),
    ]
    rows = []
    with tempfile.TemporaryDirectory(prefix='eagle-logger-compare-') as tmp:
        for name, dependencies, setup in variants:
            project = Path(tmp) / name
            (project / 'src').mkdir(parents=True)
            (project / 'Cargo.toml').write_text('[package]\nname = "probe-' + name + '"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n[dependencies]\n' + dependencies + '\n[profile.release]\nlto="fat"\ncodegen-units=1\npanic="abort"\nstrip=true\n')
            event = '' if name == 'empty' else 'log::info!(provider = "fixture", track_id = 42; "benchmark event");'
            source = 'fn main() { let dir = std::env::var("EAGLE_DATA_DIR").unwrap(); ' + setup
            source += ' let info = std::env::args().any(|a| a == "info"); loop { if info { ' + event
            source += ' std::thread::sleep(std::time::Duration::from_secs(1)); } else { std::thread::park(); } } }'
            (project / 'src/main.rs').write_text(source)
            subprocess.run(['cargo', 'build', '--release', '--manifest-path', str(project / 'Cargo.toml')], check=True)
            binary = project / ('target/release/probe-' + name + ('.exe' if system == 'Windows' else ''))
            for scenario in (['idle'] if name == 'empty' else ['idle', 'info']):
                data = project / ('data-' + scenario)
                data.mkdir()
                child = subprocess.Popen([str(binary), scenario], stderr=subprocess.DEVNULL,
                                         env=dict(os.environ, EAGLE_DATA_DIR=str(data), EAGLE_LOG='info'))
                try:
                    time.sleep(1.25)
                    start = sample(child.pid)
                    before = time.monotonic()
                    time.sleep(args.seconds)
                    end = sample(child.pid)
                    elapsed = time.monotonic() - before
                    if child.poll() is not None:
                        raise RuntimeError('Probe exited during measurement')
                    switches = '' if end[2] is None else sum(row[3] for row in thread_deltas(start[2], end[2]) if row[3] != '')
                    row = [name, scenario, system, platform.machine(), elapsed, binary.stat().st_size,
                           (end[0] - start[0]) / elapsed * 100, end[1], switches]
                    rows.append(row)
                    print(row, flush=True)
                finally:
                    child.terminate()
                    child.wait(timeout=10)
    output = root / 'bench/results' / ('logger-comparison-' + datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '.csv')
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open('w', newline='') as stream:
        writer = csv.writer(stream)
        writer.writerow(['backend', 'scenario', 'os', 'arch', 'duration_seconds', 'binary_bytes',
                         'cpu_percent_one_core', metric, 'voluntary_switches_observed'])
        writer.writerows(rows)
    print(output)


if __name__ == '__main__':
    main()
