# SPDX-License-Identifier: GPL-3.0-or-later
"""Checks for the desktop footprint report (no GUI required)."""
import unittest
from desktop import percentile, summarize


class DesktopReportTest(unittest.TestCase):
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
