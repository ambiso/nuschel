#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Device-free independent libopus vs safe-Rust SILK, DTX and framing audit.

The library is a test oracle only. No servers, host devices or Cargo builds.
Compile opus_silk_probe.rs against the vendor crate; see docs/opus-silk.md.
"""
import argparse
import ctypes as C
import hashlib
import json
import math
from pathlib import Path
import struct
import subprocess


class Reference:
    def __init__(self, library):
        self.lib = C.CDLL(str(library))
        ptr, integer = C.c_void_p, C.c_int
        self.lib.opus_encoder_create.argtypes = [integer, integer, integer, C.POINTER(integer)]
        self.lib.opus_encoder_create.restype = ptr
        self.lib.opus_decoder_create.argtypes = [integer, integer, C.POINTER(integer)]
        self.lib.opus_decoder_create.restype = ptr
        for kind in ['encoder', 'decoder']:
            getattr(self.lib, f'opus_{kind}_destroy').argtypes = [ptr]
        self.lib.opus_encode_float.argtypes = [ptr, C.POINTER(C.c_float), integer, ptr, integer]
        self.lib.opus_decode_float.argtypes = [ptr, ptr, integer, C.POINTER(C.c_float), integer, integer]
        self.lib.opus_packet_parse.argtypes = [ptr, integer, ptr, C.POINTER(ptr), C.POINTER(C.c_int16), ptr]
        self.ctl = C.CFUNCTYPE(integer, ptr, integer, integer)
        self.query = C.CFUNCTYPE(integer, ptr, integer, C.POINTER(C.c_uint32))(("opus_decoder_ctl", self.lib))
        self.version = self.lib.opus_get_version_string
        self.version.restype = C.c_char_p

    def encode(self, channels, ms, fec, bandwidth=1103, application=2048, count=12):
        error = C.c_int()
        encoder = self.lib.opus_encoder_create(48000, channels, application, C.byref(error))
        assert encoder and not error.value
        ctl = self.ctl(("opus_encoder_ctl", self.lib))
        try:
            # Explicit controls: CBR, forced stream channels/bandwidth, FEC/loss.
            # Test-only force-mode control from libopus src/opus_private.h;
            # without it the encoder may choose CELT even at wideband.
            mode = 1002 if application == 2051 else (1000 if bandwidth <= 1103 else 1001)
            for request, value in [(4002, 24000 * channels), (4006, 0), (4022, channels), (11002, mode),
                                   (4008, bandwidth), (4012, int(fec)), (4014, 25 if fec else 0)]:
                assert ctl(encoder, request, value) == 0
            frame = int(ms * 48)
            packets = []
            for packet in range(count):
                samples = []
                for sample in range(frame):
                    t = (packet * frame + sample) / 48000
                    for channel in range(channels):
                        value = .22 * math.sin(2 * math.pi * (173 + 61 * channel) * t)
                        value += .06 * math.sin(2 * math.pi * (827 + 107 * channel) * t)
                        samples.append(value * (1 + .2 * math.sin(2 * math.pi * 3.1 * t)))
                pcm = (C.c_float * len(samples))(*samples)
                output = C.create_string_buffer(4096)
                size = self.lib.opus_encode_float(encoder, pcm, frame, output, len(output))
                assert size > 0
                packets.append(output.raw[:size])
            return packets
        finally:
            self.lib.opus_encoder_destroy(encoder)

    def payload(self, packet):
        data = C.create_string_buffer(packet)
        frames = (C.c_void_p * 48)()
        sizes = (C.c_int16 * 48)()
        count = self.lib.opus_packet_parse(data, len(packet), None, frames, sizes, None)
        assert count == 1
        return C.string_at(frames[0], sizes[0])

    def decode(self, events, rate, channels):
        error = C.c_int()
        decoder = self.lib.opus_decoder_create(rate, channels, C.byref(error))
        assert decoder and not error.value
        ctl = self.ctl(("opus_decoder_ctl", self.lib))
        assert ctl(decoder, 4046, 1) == 0
        records, samples = [], []
        try:
            for operation, frames, packet in events:
                if operation == 'reset':
                    assert ctl(decoder, 4028, 0) == 0
                    final = C.c_uint32()
                    assert self.query(decoder, 4031, C.byref(final)) == 0
                    records.append({'result': 0, 'range': final.value})
                    continue
                output = (C.c_float * (frames * channels))()
                result = self.lib.opus_decode_float(decoder, packet or None, len(packet), output,
                                                   frames, int(operation == 'fec'))
                final = C.c_uint32()
                assert self.query(decoder, 4031, C.byref(final)) == 0
                records.append({'result': result, 'range': final.value})
                if result > 0:
                    samples.extend(output[:result * channels])
            return records, samples
        finally:
            self.lib.opus_decoder_destroy(decoder)


def metrics(expected, observed):
    assert len(expected) == len(observed) and expected
    error = sum((a - b) ** 2 for a, b in zip(expected, observed))
    energy = sum(a * a for a in expected)
    return {'samples': len(expected), 'max_abs_error': max(abs(a-b) for a,b in zip(expected, observed)),
            'fixed_gain_snr_db': 10 * math.log10(max(energy, 1e-30) / max(error, 1e-30))}


def ordered(packets, rate, ms):
    return [('decode', int(rate * ms / 1000), packet) for packet in packets]


def read_frozen(path):
    data, at, packets = path.read_bytes(), 0, []
    while at < len(data):
        size = struct.unpack_from('<H', data, at)[0]
        at += 2
        packets.append(data[at:at + size])
        at += size
    assert at == len(data)
    return packets


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--opus-library', type=Path, required=True)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--frozen', type=Path, help='original frozen 24-packet failure')
    parser.add_argument('--suite', choices=['all', 'regressions', 'audit'], default='all',
                        help='regressions uses the receive 48k API; audit also checks lower API rates')
    parser.add_argument('--require-parity', action='store_true')
    args = parser.parse_args()
    if args.suite == 'regressions' and args.require_parity and not args.frozen:
        parser.error('the strict receive regression gate requires the original --frozen fixture')
    args.output.mkdir(parents=True, exist_ok=True)
    ref = Reference(args.opus_library)
    cases = []

    def check(name, events, rate=48000, channels=2, threshold=35):
        expected_records, expected = ref.decode(events, rate, channels)
        event_path = args.output / f'{name}.events'
        event_path.write_text(''.join(f'{op} {frames} {packet.hex() or "-"}\n' for op,frames,packet in events))
        pcm_path, record_path = args.output / f'{name}.f32le', args.output / f'{name}.json'
        subprocess.run([str(args.probe), str(rate), str(channels), str(event_path), str(pcm_path), str(record_path)],
                       check=True, capture_output=True, timeout=20)
        observed_records = json.loads(record_path.read_text())
        pcm = pcm_path.read_bytes()
        observed = struct.unpack(f'<{len(pcm)//4}f', pcm)
        result = {'name': name, 'rate': rate, 'channels': channels,
                  'reference_records': expected_records, 'rust_records': observed_records,
                  'packet_sha256': [hashlib.sha256(p).hexdigest() for _,_,p in events]}
        result['results_equal'] = [r['result'] for r in expected_records] == [r['result'] for r in observed_records]
        result['ranges_equal'] = [r['range'] for r in expected_records] == [r['range'] for r in observed_records]
        result['pcm'] = metrics(expected, observed) if len(expected) == len(observed) else None
        result['passed'] = (result['results_equal'] and result['ranges_equal'] and result['pcm'] is not None
                            and (result['pcm']['max_abs_error'] == 0 or result['pcm']['fixed_gain_snr_db'] >= threshold))
        cases.append(result)
        return expected_records, expected, observed_records, observed

    if args.frozen:
        assert hashlib.sha256(args.frozen.read_bytes()).hexdigest() == 'ee9fdd1411600fedbad7535ae58401cfb51ba638fd7b7d4903647125fa401e85'
        packets = read_frozen(args.frozen)
        check('frozen-ordered-fec-enabled', ordered(packets, 48000, 20))
        assert len(packets) == 24
        assert ref.payload(packets[2]) == b'\x00' and ref.payload(packets[22]) == b'\x00'

    # Rate/channel/frame-duration matrix, independent FEC on/off encoder streams.
    for stream_channels in [1, 2]:
        for ms in [10, 20, 40, 60]:
            for fec in [False, True]:
                packets = ref.encode(stream_channels, ms, fec)
                assert all(packet[0] >> 3 < 12 for packet in packets), 'fixture must use SILK'
                rates = ([8000, 12000, 16000, 24000, 48000]
                         if args.suite != 'regressions' and (stream_channels == 1 or ms == 20) else [48000])
                for rate in rates:
                    for channels in [1, 2]:
                        tag = f's{stream_channels}-ms{ms}-fec{int(fec)}-r{rate}-c{channels}'
                        check('ordered-' + tag, ordered(packets, rate, ms), rate, channels)
                        # A real lost frame: recover from next packet then decode it normally.
                        events = ordered(packets, rate, ms)
                        events[4] = ('fec', int(rate * ms / 1000), packets[5])
                        check('loss-' + tag, events, rate, channels)
                        events[4] = ('decode', int(rate * ms / 1000), b'')
                        check('plc-' + tag, events, rate, channels)

    # NB/MB use different SILK internal clocks/resamplers.
    for bandwidth in [1101, 1102]:
        for ms in [10, 20, 40, 60]:
            packets = ref.encode(1, ms, True, bandwidth)
            check(f'bandwidth-{bandwidth}-ms{ms}', ordered(packets, 48000, ms))

    # Explicit short coded payloads: padding must not change their DTX meaning.
    # Include first-ever frames, mode/channel switches, mixed frame lists and
    # subsequent ordinary packets so incorrect hidden state is observable.
    for kind, app, bandwidth, ms in [('silk', 2048, 1103, 20), ('celt', 2051, 1105, 20),
                                      ('hybrid', 2048, 1105, 20)]:
        packets = ref.encode(2, ms, True, bandwidth, app)
        if kind == 'hybrid':
            assert all(12 <= p[0] >> 3 < 16 for p in packets), 'fixture must use hybrid'
        check(f'ordered-{kind}-control', ordered(packets, 48000, ms))
        lost = ordered(packets, 48000, ms)
        lost[4] = ('fec', 960, packets[5])
        check(f'loss-{kind}-control', lost)
        if kind == 'hybrid':
            no_lbrr = ref.encode(2, ms, False, bandwidth, app)
            lost = ordered(no_lbrr, 48000, ms)
            lost[4] = ('fec', 960, no_lbrr[5])
            check('loss-hybrid-no-lbrr', lost)
            lost[4] = ('decode', 960, b'')
            check('plc-hybrid-control', lost)
        payload = ref.payload(packets[5])
        toc = packets[5][0] & ~3
        for short in [b'', b'\x00', b'\xff']:
            padded = bytes([toc | 3, 0x41, 5]) + short + b'\0' * 5
            single = bytes([toc]) + short
            for packet in [single, padded]:
                tag = f'{kind}-short{len(short)}-{short.hex()}-pad{int(packet == padded)}'
                check(tag, ordered(packets[:4] + [packet] + packets[5:], 48000, ms))
                check('first-' + tag, ordered([packet] + packets, 48000, ms))
                check('fec-' + tag, ordered(packets[:4], 48000, ms) + [('fec', 960, packet)] + ordered(packets[5:], 48000, ms))
            assert len(payload) < 252
            for first, second in [(short, payload), (payload, short)]:
                mixed = bytes([toc | 2, len(first)]) + first + second
                check(f'mixed-{kind}-{short.hex()}-{int(first == short)}',
                      ordered(packets[:4], 48000, ms) + [('decode', 1920, mixed)] + ordered(packets[6:], 48000, ms))

    # A structurally invalid future packet must not run PLC or mutate state.
    # Test each bypass: previous CELT mode, future CELT mode, too-short FEC and
    # leading PLC before a malformed SILK packet. Follow with a valid packet.
    celt = ref.encode(2, 20, False, 1105, 2051)
    silk = ref.encode(1, 20, True)
    for before, after in [(celt, silk), (silk, celt)]:
        for short in [b'', b'\x00']:
            packet = bytes([after[0][0] & ~3]) + short
            check(f'dtx-mode-switch-{before[0][0]}-{len(short)}',
                  ordered(before[:4] + [packet] + before[4:7] + after[4:7], 48000, 20))
            check(f'dtx-mode-switch-fec-{before[0][0]}-{len(short)}',
                  ordered(before[:4] + [packet], 48000, 20) + [('fec', 960, before[5])]
                  + ordered(before[5:8], 48000, 20))
            check(f'reset-dtx-mode-fec-{before[0][0]}-{len(short)}',
                  ordered(before[:4], 48000, 20) + [('reset', 0, b'')]
                  + ordered([packet], 48000, 20) + [('fec', 960, before[5])]
                  + ordered(before[5:8], 48000, 20))
    for kind, app, bandwidth in [('silk', 2048, 1103), ('celt', 2051, 1105)]:
        mono = ref.encode(1, 20, True, bandwidth, app)
        stereo = ref.encode(2, 20, True, bandwidth, app)
        for before, after in [(mono, stereo), (stereo, mono)]:
            for short in [b'', b'\x00']:
                packet = bytes([after[0][0] & ~3]) + short
                check(f'dtx-channel-{kind}-{before[0][0]}-{len(short)}',
                      ordered(before[:4] + [packet], 48000, 20) + [('fec', 960, after[5])]
                      + ordered(after[5:8], 48000, 20))
    malformed = [b'\x09\x00', b'\x99\x00', b'\x4b', b'\x4b\x00', b'\x4b\x41\xff', b'\x4a\xfc']
    for prefix_name, prefix in [('celt', celt), ('silk', silk)]:
        for index, packet in enumerate(malformed):
            for request in [480, 960, 1920]:
                events = ordered(prefix[:4], 48000, 20) + [('fec', request, packet)] + ordered(prefix[4:7], 48000, 20)
                records, expected, observed_records, observed = check(f'malformed-{prefix_name}-{index}-{request}', events)
                assert records[4]['result'] == -4
                clean_records, clean = ref.decode(ordered(prefix[:7], 48000, 20), 48000, 2)
                assert expected == clean and records[4]['range'] == records[3]['range']
                # This explicitly checks Rust error/state preservation as well as oracle parity.
                clean_name = f'clean-{prefix_name}-{index}-{request}'
                clean_events = args.output / f'{clean_name}.events'
                clean_events.write_text(''.join(f'{op} {frames} {p.hex()}\n'
                                               for op,frames,p in ordered(prefix[:7], 48000, 20)))
                clean_pcm = args.output / f'{clean_name}.f32le'
                clean_json = args.output / f'{clean_name}.json'
                subprocess.run([str(args.probe), '48000', '2', str(clean_events), str(clean_pcm), str(clean_json)],
                               check=True, capture_output=True, timeout=20)
                cases[-1]['state_unchanged'] = (args.output / (cases[-1]['name'] + '.f32le')).read_bytes() == clean_pcm.read_bytes()
                cases[-1]['state_unchanged'] &= observed_records[4]['range'] == observed_records[3]['range']
                cases[-1]['passed'] &= cases[-1]['state_unchanged']

    proof = {'reference': str(args.opus_library), 'version': ref.version().decode(),
             'cases': cases, 'passed': sum(c['passed'] for c in cases), 'total': len(cases),
             'limits': ['Synthetic codec fixtures only; no browser/native integration or listening proof.',
                        'This checks bounded SILK/DTX/framing behavior, not general Opus conformance.']}
    (args.output / 'proof.json').write_text(json.dumps(proof, indent=2) + '\n')
    failures = [c['name'] for c in cases if not c['passed']]
    print(json.dumps({'passed': proof['passed'], 'total': proof['total'],
                      'failures': failures[:8], 'failure_count': len(failures)}))
    if args.require_parity:
        assert proof['passed'] == proof['total'], 'independent reference mismatch; see proof.json'


if __name__ == '__main__':
    main()
