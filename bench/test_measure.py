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
        deadline = time.monotonic() + 0.15
        while time.monotonic() < deadline:
            pass
        after = sampler(os.getpid())
        self.assertGreater(after[0], before[0])
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


if __name__ == '__main__':
    unittest.main()
