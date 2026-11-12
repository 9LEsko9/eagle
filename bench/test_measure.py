# SPDX-FileCopyrightText: 2026 eagle contributors
# SPDX-License-Identifier: GPL-3.0-or-later

"""Contract checks for the native sampling ABI on the current host."""
import os
import platform
import time
import unittest
import tempfile
from pathlib import Path

import measure


class SamplerTest(unittest.TestCase):
    def test_process_cpu_and_memory(self):
        sampler = {'Darwin': measure.mac_sample, 'Linux': measure.linux_sample,
                   'Windows': measure.windows_sample}[platform.system()]
        before = sampler(os.getpid())
        process_before = time.process_time()
        deadline = time.monotonic() + 0.15
        while time.monotonic() < deadline:
            pass
        process_elapsed = time.process_time() - process_before
        after = sampler(os.getpid())
        self.assertGreater(after[0], before[0])
        self.assertGreater(after[0] - before[0], process_elapsed * 0.7)
        self.assertLess(after[0] - before[0], process_elapsed * 1.3)
        self.assertGreater(after[1], 0)
        if after[2] is not None:
            self.assertTrue(measure.thread_deltas(before[2], after[2]))


class ThreadDeltaTest(unittest.TestCase):
    def test_linux_reads_only_voluntary_for_each_thread(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            for tid, voluntary in [('10', 4), ('11', 7)]:
                task = base / 'task' / tid
                task.mkdir(parents=True)
                (task / 'comm').write_text('eg-worker\n')
                (task / 'status').write_text(
                    f'voluntary_ctxt_switches: {voluntary}\nnonvoluntary_ctxt_switches: 999\n')
                (task / 'stat').write_text(tid + ' (eg-worker) ' + ' '.join(['0'] * 20))
            snapshot = measure.linux_threads(base)
            self.assertEqual(sum(v[1] for v in snapshot.values()), 11)
            self.assertEqual(len(snapshot), 2)

    def test_all_threads_and_partial_coverage(self):
        start = {('1', '100'): ('main', 9), ('2', '101'): ('eg-audio', 20),
                 ('3', '102'): ('eg-net', 1)}
        end = {('1', '100'): ('main', 10), ('2', '101'): ('eg-audio', 25),
               ('4', '103'): ('eg-new', 2)}
        rows = measure.thread_deltas(start, end)
        self.assertEqual([r[3] for r in rows], [1, 5, '', 2])
        self.assertEqual([r[4] for r in rows],
                         ['full_window', 'full_window', 'exited', 'new_partial_window'])


class NetworkStatsTests(unittest.TestCase):
    def test_network_counters_are_required_and_parsed(self):
        self.assertEqual(measure.network_stats('Сеть: 7 запросов, 3713301 байт ответов\n'), (7, 3713301))
        self.assertEqual(measure.network_stats('Сеть: 0 запросов, 0 байт ответов'), (0, 0))
        with self.assertRaises(ValueError):
            measure.network_stats('Error: Network')


class PlaybackCacheTests(unittest.TestCase):
    def test_incomplete_audio_is_rejected_before_starting_cli(self):
        import json
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / 'audio'
            cache.mkdir()
            (cache / 'yandex_123.high').write_bytes(b'test')
            (cache / 'yandex_123.high.map').write_text(json.dumps(
                {'total': 4, 'ranges': [[0, 2]]}))
            with patch('measure.subprocess.Popen') as spawn:
                with self.assertRaises(ValueError):
                    measure.measure_playback(Path('/unused'), directory, 'yandex:123',
                                             1, None, 'memory')
                spawn.assert_not_called()


class WakeupDeltaTests(unittest.TestCase):
    def test_native_counters_are_separate_from_radio_wakeups(self):
        start = (0, 0, None, {'package_idle_wakeups': 2, 'interrupt_wakeups': 7})
        end = (0, 0, None, {'package_idle_wakeups': 5, 'interrupt_wakeups': 11})
        self.assertEqual(measure.wakeup_deltas(start, end), [3, 4])
        self.assertEqual(measure.wakeup_deltas((0, 0, None), (0, 0, None)), ['', ''])
        with self.assertRaises(ValueError):
            measure.wakeup_deltas(end, start)


if __name__ == '__main__':
    unittest.main()
