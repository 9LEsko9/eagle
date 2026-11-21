# SPDX-License-Identifier: GPL-3.0-or-later
"""Checks for the desktop footprint report (no GUI required)."""
import unittest
import json
import sqlite3
import plistlib
import signal
import subprocess
import sys
from types import SimpleNamespace
from unittest.mock import patch
from contextlib import closing
from pathlib import Path
from tempfile import TemporaryDirectory
from desktop import app_command, completed_peak, fixture_queue_entries, observed_gpu_api, percentile, scroll_metrics, summarize, terminate_app


class DesktopReportTest(unittest.TestCase):
    def test_invalid_scroll_step_is_rejected_before_creating_fixture_files(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for value in ('0', '-1', '65536'):
                result = subprocess.run([
                    sys.executable, str(Path(__file__).with_name('desktop.py')),
                    '--binary', str(root / 'missing-binary'),
                    '--fixture-root', str(root / 'fixture'),
                    '--output', str(root / 'report.json'),
                    '--scroll-step', value,
                ], capture_output=True, text=True)
                self.assertEqual(result.returncode, 2)
                self.assertIn('scroll-step must be in 1..65535', result.stderr)
                self.assertEqual(list(root.iterdir()), [])

    def test_scroll_samples_exclude_other_stages_and_preserve_zero_frame_counts(self):
        log = '''average frames per second: 90 details from last frame: [2 layers created]
Eagle fixture stage: idle
average frames per second: 10
Eagle fixture stage: scrolling
average frames per second: 0
average frames per second: 57 details from last frame: [3 layers created]
Eagle fixture stage: playing
average frames per second: 60 details from last frame: [1 layers created]
'''
        self.assertEqual(scroll_metrics(log), [
            {'rendered_frames': 0, 'layers_created_last_frame': None},
            {'rendered_frames': 57, 'layers_created_last_frame': 3},
        ])

    def test_app_launch_preserves_arguments_and_high_resolution_metadata(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'binary with spaces'
            binary.write_bytes(b'fixture binary')
            output = root / 'report.json'
            arguments = ['--fixture-root', '/tmp/Музыка с пробелами', '--fixture-smoke']
            with patch('desktop.app_pid', return_value=None):
                command, executable = app_command(binary, output, {
                    'EAGLE_DATA_DIR': '/tmp/isolated',
                    'SLINT_DEBUG_PERFORMANCE': 'refresh_lazy,console',
                }, arguments)
            self.assertEqual(command[-len(arguments):], arguments)
            self.assertIn('SLINT_DEBUG_PERFORMANCE=refresh_lazy,console', command)
            self.assertEqual(executable.read_bytes(), binary.read_bytes())
            with (output.with_suffix('.app') / 'Contents' / 'Info.plist').open('rb') as file:
                self.assertTrue(plistlib.load(file)['NSHighResolutionCapable'])

    def test_app_cleanup_never_signals_a_recycled_or_unrelated_pid(self):
        with patch('desktop.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout='123\n')):
            with patch('desktop.os.kill') as kill:
                terminate_app(456, Path('/tmp/our-benchmark'))
                kill.assert_not_called()
                terminate_app(123, Path('/tmp/our-benchmark'))
                kill.assert_called_once_with(123, signal.SIGTERM)

    def test_gpu_backend_is_observed_instead_of_inferred_from_requested_flags(self):
        self.assertEqual(observed_gpu_api('femtovg device backend=Metal\n' * 4), 'WGPU/Metal')
        for log in ('', 'femtovg device backend=Gl',
                    'femtovg device backend=Metal\nfemtovg device backend=Vulkan'):
            with self.assertRaises(ValueError):
                observed_gpu_api(log)

    def test_reused_fixture_cannot_silently_shrink_the_queue(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            with closing(sqlite3.connect(root / 'metadata.sqlite')) as db, db:
                db.execute('CREATE TABLE playback(json TEXT)')
                db.execute('CREATE TABLE liked(grp INTEGER)')
                db.execute('CREATE TABLE downloads(track TEXT, state TEXT)')
                db.execute('CREATE TABLE offline_owners(track TEXT)')
                db.executemany('INSERT INTO liked VALUES(?)', ((n,) for n in range(10_000)))
                db.executemany('INSERT INTO downloads VALUES(?,?)',
                               ((str(n), 'saved') for n in range(1_000)))
                db.executemany('INSERT INTO offline_owners VALUES(?)',
                               ((str(n),) for n in range(1_000)))
                db.execute('INSERT INTO playback VALUES(?)',
                           [json.dumps(dict(entries=[0] * 1000, order=[0] * 1000))])
            with self.assertRaises(ValueError):
                fixture_queue_entries(root)
            with closing(sqlite3.connect(root / 'metadata.sqlite')) as db, db:
                db.execute('UPDATE playback SET json=?',
                           [json.dumps(dict(entries=[0] * 10_000, order=[0] * 10_000))])
            self.assertEqual(fixture_queue_entries(root), 10_000)
            for table in ('liked', 'downloads', 'offline_owners'):
                with closing(sqlite3.connect(root / 'metadata.sqlite')) as db, db:
                    row = db.execute(f'SELECT * FROM {table} WHERE rowid=1').fetchone()
                    db.execute(f'DELETE FROM {table} WHERE rowid=1')
                with self.assertRaises(ValueError):
                    fixture_queue_entries(root)
                with closing(sqlite3.connect(root / 'metadata.sqlite')) as db, db:
                    placeholders = ','.join('?' for _ in row)
                    db.execute(f'INSERT INTO {table} VALUES({placeholders})', row)
                self.assertEqual(fixture_queue_entries(root), 10_000)

    def test_completed_peak_rejects_stale_or_invalid_counters(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / 'peak'
            with self.assertRaises(FileNotFoundError):
                completed_peak(path, 10)
            for value in ('0', '-1', '9', 'invalid'):
                path.write_text(value)
                with self.assertRaises(ValueError):
                    completed_peak(path, 10)
            path.write_text('15\n')
            self.assertEqual(completed_peak(path, 10), 15)

    def test_percentiles_and_stage_plateaus_do_not_mix_startup(self):
        self.assertEqual(percentile(list(range(1, 101)), .95), 95)
        series = [dict(stage='startup', seconds=0, phys_footprint_bytes=200),
                  dict(stage='idle', seconds=1, phys_footprint_bytes=100),
                  dict(stage='idle', seconds=2, phys_footprint_bytes=50),
                  dict(stage='idle', seconds=3, phys_footprint_bytes=60)]
        stages = summarize(series)
        self.assertEqual(stages['idle']['sampled_peak_bytes'], 100)
        self.assertEqual(stages['idle']['settled_final_bytes'], 60)
        self.assertEqual(stages['idle']['settled_last_second_p95_bytes'], 60)
        self.assertEqual(stages['startup']['samples'], 1)


if __name__ == '__main__':
    unittest.main()
