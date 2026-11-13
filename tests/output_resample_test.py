#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The wide mixer dump is read in the layout its writer produces (issue #225)."""
from pathlib import Path
import struct
import tempfile
import unittest
from output_resample import device_clip, read_mix_dump


class MixDump(unittest.TestCase):
    def test_stereo_f64_frames_are_read_exactly_and_f32_is_refused(self):
        frames = [1.0374, -0.25, 0.5, -1.5]  # two stereo frames with headroom
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'native.f64le'
            path.write_bytes(struct.pack('<4d', *frames))
            self.assertEqual(read_mix_dump(path, 2), tuple(frames))
            # The same frames as f32 have half the size: refused, never misread.
            path.write_bytes(struct.pack('<4f', *frames))
            with self.assertRaises(ValueError):
                read_mix_dump(path, 2)

    def test_the_device_clip_is_unit_amplitude_f32(self):
        self.assertEqual(device_clip([1.0374, -1.5, 0.5]), (1., -1., .5))
        self.assertEqual(device_clip([.1]), struct.unpack('<f', struct.pack('<f', .1)))


if __name__ == '__main__':
    unittest.main()
