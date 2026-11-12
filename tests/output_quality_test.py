#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Verify the independent measurement cannot hide gain/stereo failures."""
import math
import unittest
from output_quality import amplitude, compare, delay_only, fixture


class Measurements(unittest.TestCase):
    def test_fixed_gain_does_not_normalize_attenuation_away(self):
        source = [math.sin(i * .171) * .2 for i in range(4096)]
        actual = [v * .5 for v in source]
        result = compare(source, actual)
        self.assertAlmostEqual(result['diagnostic_gain'], .5)
        self.assertAlmostEqual(result['fixed_gain_snr_db'], 20 * math.log10(2))
        self.assertEqual(compare(source, source)['max_abs_error'], 0)

    def test_delay_alignment_cannot_hide_a_swapped_or_downmixed_channel(self):
        stereo = fixture(2)
        left, right = stereo[::2], stereo[1::2]
        lag = 15317
        self.assertEqual(delay_only(left, [0.] * lag + left + [0.] * 4800), lag)
        mono = [(a + b) / 2 for a, b in zip(left, right)]
        self.assertLess(compare(stereo, [v for sample in mono for v in (sample, sample)])['fixed_gain_snr_db'], 4)
        self.assertGreater(amplitude(left[24000:48000], 997), amplitude(right[24000:48000], 997) * 100)

    def test_nonfinite_or_partial_comparisons_are_rejected(self):
        for expected, actual in [([], []), ([1.], []), ([float('nan')], [0.])]:
            with self.assertRaises(ValueError):
                compare(expected, actual)


if __name__ == '__main__':
    unittest.main()
