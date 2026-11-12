#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent device-free libopus comparison of CELT PLC frame bookkeeping.

Reference libopus is TEST ONLY. All application decoding remains safe Rust.
The strict gate uses unity gain, no fitted delay, and both fresh/reused scratch.
"""
import argparse
import ctypes as C
import hashlib
import json
import math
from pathlib import Path
import struct
import subprocess

from opus_silk_fec import Reference, metrics, ordered


class CeltReference(Reference):
    def encode_celt(self, channels, ms, count=48):
        error = C.c_int()
        encoder = self.lib.opus_encoder_create(48000, channels, 2051, C.byref(error))
        assert encoder and not error.value
        ctl = self.ctl(("opus_encoder_ctl", self.lib))
        try:
            for request, value in [(4002, 64000), (4006, 0), (4022, channels), (4008, 1105)]:
                assert ctl(encoder, request, value) == 0
            frame = int(ms * 48)
            packets = []
            for index in range(count):
                values = []
                for sample in range(frame):
                    t = (index * frame + sample) / 48000
                    for channel in range(channels):
                        value = .2 * math.sin(2 * math.pi * (997 + channel * 1006) * t)
                        value += .04 * math.sin(2 * math.pi * (317 + channel * 296) * t)
                        values.append(value * (1 + .2 * math.sin(2 * math.pi * 3.1 * t)))
                pcm = (C.c_float * len(values))(*values)
                buffer = C.create_string_buffer(1275)
                size = self.lib.opus_encode_float(encoder, pcm, frame, buffer, len(buffer))
                assert size > 0
                packets.append(buffer.raw[:size])
            assert all(packet[0] >> 3 >= 16 for packet in packets)
            return packets
        finally:
            self.lib.opus_encoder_destroy(encoder)

    def aggregate(self, packets):
        ptr, integer = C.c_void_p, C.c_int
        self.lib.opus_repacketizer_create.restype = ptr
        self.lib.opus_repacketizer_destroy.argtypes = [ptr]
        self.lib.opus_repacketizer_cat.argtypes = [ptr, ptr, integer]
        self.lib.opus_repacketizer_out.argtypes = [ptr, ptr, integer]
        rp = self.lib.opus_repacketizer_create()
        assert rp
        try:
            for packet in packets:
                assert self.lib.opus_repacketizer_cat(rp, packet, len(packet)) == 0
            buffer = C.create_string_buffer(16384)
            count = self.lib.opus_repacketizer_out(rp, buffer, len(buffer))
            assert count > 0
            return buffer.raw[:count]
        finally:
            self.lib.opus_repacketizer_destroy(rp)


def samples(path):
    raw = path.read_bytes()
    assert len(raw) % 4 == 0
    return struct.unpack(f'<{len(raw)//4}f', raw)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--opus-library', required=True, type=Path)
    parser.add_argument('--probe', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--frozen-events', required=True, type=Path,
                        help='original 120ms-then-missing20ms.events admission fixture')
    parser.add_argument('--require-parity', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = CeltReference(args.opus_library)
    cases = []

    def check(name, events, channels=2):
        records, expected = reference.decode(events, 48000, channels)
        path = args.output / f'{name}.events'
        path.write_text(''.join(f'{op} {frames} {packet.hex() or "-"}\n' for op,frames,packet in events))
        result = {'name': name, 'channels': channels, 'reference_records': records,
                  'packets': [hashlib.sha256(packet).hexdigest() for _,_,packet in events], 'buffers': {}}
        previous = None
        for scratch in ['fresh', 'reuse']:
            pcm = args.output / f'{name}-{scratch}.f32le'
            record = args.output / f'{name}-{scratch}.json'
            subprocess.run([str(args.probe), '48000', str(channels), str(path), str(pcm), str(record), scratch],
                           check=True, capture_output=True, timeout=20)
            observed_records = json.loads(record.read_text())
            observed = samples(pcm)
            m = metrics(expected, observed) if len(expected) == len(observed) else None
            passed = (records == observed_records and m is not None
                      and (m['max_abs_error'] == 0 or m['fixed_gain_snr_db'] >= 35))
            if previous is not None:
                passed &= previous == pcm.read_bytes()
            previous = pcm.read_bytes()
            result['buffers'][scratch] = {'records': observed_records, 'pcm': m, 'passed': passed}
        result['passed'] = all(value['passed'] for value in result['buffers'].values())
        cases.append(result)
        return result

    # Preserve/replay exactly the received bytes from the independent failure.
    frozen = args.frozen_events.read_bytes()
    assert hashlib.sha256(frozen).hexdigest() == 'f4aa72601e6c82c1955d26fc0497485904bd4ca76910fa53e5a157cd27372c53'
    pushes = [bytes.fromhex(line.split()[5]) for line in frozen.decode().splitlines() if line.startswith('P ')]
    assert len(pushes) == 2
    assert (pushes[0][0] >> 3) == 30  # 10 ms CELT TOC, 120 ms total packet
    for operation in ['decode', 'fec']:
        for gap_ms in [10, 20]:
            gap_packet = pushes[1] if operation == 'fec' else b''
            check(f'frozen-{operation}-{gap_ms}', [('decode', 5760, pushes[0]),
                  (operation, gap_ms * 48, gap_packet), ('decode', 960, pushes[1])])

    # Complete 10/20 ms internal-frame packet lengths up to 120 ms, with real
    # 10/20 ms loss. The CELT FEC entry point must conceal the same gap.
    for stream_channels in [1, 2]:
        for frame_ms in [10, 20]:
            packets = reference.encode_celt(stream_channels, frame_ms)
            for count in range(1, 120 // frame_ms + 1):
                grouped = reference.aggregate(packets[:count])
                following = packets[count + 2]
                for channels in [1, 2]:
                    for gap_ms in [10, 20]:
                        for operation in ['decode', 'fec']:
                            tag = f's{stream_channels}-c{channels}-internal{frame_ms}-packet{count*frame_ms}-gap{gap_ms}-{operation}'
                            gap_packet = following if operation == 'fec' else b''
                            check(tag, [('decode', count * frame_ms * 48, grouped),
                                  (operation, gap_ms * 48, gap_packet)]
                                  + ordered(packets[count+2:count+6], 48000, frame_ms), channels)

            # DTX has its own accepted TOC frame duration; it must update the
            # next PLC cap even when synthesis remains in the previous mode.
            other_ms = 30 - frame_ms
            other = reference.encode_celt(stream_channels, other_ms)
            long = reference.aggregate(packets[:120 // frame_ms])
            for short in [b'', b'\x00']:
                dtx = bytes([other[0][0] & ~3]) + short
                check(f'dtx-internal{frame_ms}-to{other_ms}-s{stream_channels}-len{len(short)}',
                      [('decode', 5760, long), ('decode', other_ms * 48, dtx), ('decode', 960, b'')]
                      + ordered(other[4:7], 48000, other_ms))
                # Reset must discard the old 10/20 ms concealment cap.
                check(f'reset-internal{frame_ms}-s{stream_channels}-len{len(short)}',
                      [('decode', 5760, long), ('reset', 0, b''), ('decode', other_ms * 48, dtx)]
                      + ordered(other[:3], 48000, other_ms) + [('decode', 960, b'')])

            # A rejected future packet must not change internal frame duration.
            for operation in ['decode', 'fec']:
                malformed = bytes([other[0][0] | 1, 0])  # odd-sized code 1 CBR
                check(f'malformed-frame-state-{frame_ms}-s{stream_channels}-{operation}',
                      [('decode', 5760, long), (operation, 960, malformed), ('decode', 960, b'')]
                      + ordered(packets[14:17], 48000, frame_ms))

            # Accepted SILK/Hybrid transitions use their own per-frame cap; the
            # old CELT packet's aggregate duration must not leak into that state.
            for bandwidth, mode in [(1103, 'silk'), (1105, 'hybrid')]:
                speech = reference.encode(stream_channels, 10, True, bandwidth)
                grouped_speech = reference.aggregate(speech[:6])
                check(f'celt{frame_ms}-to-{mode}-s{stream_channels}',
                      [('decode', 5760, long), ('decode', 2880, grouped_speech), ('fec', 960, speech[8])]
                      + ordered(speech[8:11], 48000, 10))
                check(f'{mode}-to-celt{frame_ms}-s{stream_channels}',
                      [('decode', 2880, grouped_speech), ('decode', 5760, long), ('decode', 960, b'')]
                      + ordered(packets[14:17], 48000, frame_ms))

    proof = {'reference': str(args.opus_library), 'version': reference.version().decode(),
             'frozen_events_sha256': hashlib.sha256(frozen).hexdigest(), 'cases': cases,
             'passed': sum(case['passed'] for case in cases), 'total': len(cases),
             'encoder': {'rate': 48000, 'application': 2051, 'bitrate': 64000, 'VBR': 0,
                         'forced_stream_channels': [1, 2], 'frame_ms': [10, 20]},
             'decoder': {'rate': 48000, 'channels': [1, 2], 'phase_inversion_disabled': True,
                         'buffers': ['fresh', 'reuse']},
             'limits': ['Synthetic codec checks; no device, host audio or live server use.',
                        'No full codec/perceptual parity claim; direct 8/12k resampling remains queued.']}
    (args.output / 'proof.json').write_text(json.dumps(proof, indent=2)+'\n')
    failures = [case['name'] for case in cases if not case['passed']]
    print(json.dumps({'passed': proof['passed'], 'total': proof['total'],
                      'failures': failures[:8], 'failure_count': len(failures)}))
    if args.require_parity:
        assert not failures, 'independent CELT PLC mismatch; see proof.json'


if __name__ == '__main__':
    main()
