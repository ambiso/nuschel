#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent negative controls for spectral measurements and evidence WAVs."""
from pathlib import Path
import struct
import tempfile
import unittest

import numpy as np
from output_spectral import metrics, sources, write_wav


class SpectralMeasurements(unittest.TestCase):
    def test_fixed_gain_comparison_exposes_level_and_channel_errors(self):
        source = sources()[0][1]
        baseline = metrics(source, source, 48000)
        attenuated = metrics(source, source*.5, 48000)
        swapped = metrics(source, source[:, ::-1], 48000)
        self.assertTrue(baseline['exact_float_match'])
        self.assertIsNone(baseline['fixed_gain_snr_db'])
        self.assertAlmostEqual(attenuated['fixed_gain_snr_db'], 6.0206, places=4)
        self.assertLess(swapped['fixed_gain_snr_db'], 0)
        self.assertGreater(attenuated['active_bin_abs_magnitude_error_p95_db'], 6)

    def test_one_sample_shift_is_not_fitted_away(self):
        source = sources()[0][1]
        shifted = np.vstack((np.zeros((1, 2)), source[:-1]))
        self.assertLess(metrics(source, shifted, 48000)['fixed_gain_snr_db'], 5)

    def test_upper_band_error_is_reported_even_with_low_band_content(self):
        t = np.arange(48000)/48000
        source = np.column_stack((.2*np.sin(2*np.pi*997*t), .2*np.sin(2*np.pi*2003*t)))
        changed = source + .01*np.sin(2*np.pi*18000*t)[:, None]
        result = metrics(source, changed, 48000)
        upper = next(row for row in result['bands'] if row['hz'][0] == 16000)
        voice = next(row for row in result['bands'] if row['hz'][0] == 300)
        self.assertGreater(upper['residual_energy'], voice['residual_energy']*1e5)

    def test_float_wave_preserves_quiet_residual_and_headroom_without_normalization(self):
        pcm = np.array([[1e-9, -1e-9], [1.2, -.2]], dtype='<f4')
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder)/'evidence.wav'
            write_wav(path, pcm, 44100)
            raw = path.read_bytes()
            self.assertEqual(raw[:4], b'RIFF')
            self.assertEqual(struct.unpack('<H', raw[20:22])[0], 3)
            self.assertEqual(struct.unpack('<I', raw[24:28])[0], 44100)
            np.testing.assert_array_equal(np.frombuffer(raw[44:], dtype='<f4').reshape(-1, 2), pcm)

    def test_nonfinite_pcm_is_rejected(self):
        pcm = np.ones((960, 2)); pcm[0, 0] = float('nan')
        with self.assertRaises(AssertionError): metrics(pcm, pcm, 48000)


if __name__ == '__main__':
    unittest.main()
