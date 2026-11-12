#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent Mumble phase-inversion setting comparison, reference libopus only.

No network/audio devices. Decoder CTL4046=1 matches pinned stock Mumble;
its default is not assumed equivalent. Optional Rust probe checks actual PCM.
"""
import argparse
import ctypes
import json
import math
from pathlib import Path
import struct
import subprocess

from output_quality import Reference, compare, rms
from output_resample import read_pcm, write_pcm

RATE = 48000
FRAME = 960


class PhaseReference(Reference):
    def decode_control(self, packets, disabled, reset_at=None):
        error = ctypes.c_int()
        decoder = self.lib.opus_decoder_create(RATE, 2, ctypes.byref(error))
        assert decoder and not error.value
        control = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_int, ctypes.c_int)(("opus_decoder_ctl", self.lib))
        query = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_uint32))(("opus_decoder_ctl", self.lib))
        try:
            assert control(decoder, 4046, int(disabled)) == 0  # PHASE_INVERSION_DISABLED
            samples, ranges = [], []
            for index, packet in enumerate(packets):
                if index == reset_at:
                    assert control(decoder, 4028, 0) == 0  # RESET_STATE retains settings
                pcm = (ctypes.c_float * 11520)()
                frames = self.lib.opus_decode_float(decoder, packet, len(packet), pcm, 5760, 0)
                assert frames == FRAME
                samples.extend(pcm[:frames * 2])
                final = ctypes.c_uint32()
                assert query(decoder, 4031, ctypes.byref(final)) == 0  # GET_FINAL_RANGE
                ranges.append(final.value)
            return samples, ranges
        finally:
            self.lib.opus_decoder_destroy(decoder)


def fixture(kind):
    # Anti-correlated high-band energy exercises the intensity inversion flag;
    # a common low-band carrier keeps mono-fold metrics nonzero in either mode.
    return [value for i in range(RATE) for value in (
        .07 * math.sin(2 * math.pi * 997 * i / RATE) + (.3 * math.sin(2 * math.pi * 11731 * i / RATE) if kind == 'intensity' else .2 * math.sin(2 * math.pi * 2003 * i / RATE)),
        .07 * math.sin(2 * math.pi * 997 * i / RATE) - (.3 * math.sin(2 * math.pi * 11731 * i / RATE) if kind == 'intensity' else .2 * math.sin(2 * math.pi * 2003 * i / RATE)))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--opus-library', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--probe-default', type=Path, help='published Rust decoder, without new control')
    parser.add_argument('--probe', type=Path, help='device-free actual Rust decoder probe')
    parser.add_argument('--require-stock', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = PhaseReference(args.opus_library)
    cases = []
    for kind, bitrate in [('intensity',16000),('intensity',24000),('intensity',32000),('intensity',48000),('low-band-control',128000)]:
        name = f'{kind}-{bitrate}'
        packets = reference.encode(fixture(kind), 2, bitrate)
        default, ranges = reference.decode_control(packets, False)
        stock, stock_ranges = reference.decode_control(packets, True)
        assert ranges == stock_ranges, 'control must not alter entropy consumption'
        left = lambda data: data[RATE//5*2::2]
        fold = lambda data: [(a+b)*.5 for a,b in zip(data[RATE//5*2::2],data[RATE//5*2+1::2])]
        metrics = {'name':name,'bitrate':bitrate,'packets':len(packets),
            'toc_configs':sorted(set(packet[0] >> 3 for packet in packets)),
            'default_vs_stock':compare(stock,default),
            'default_mono_fold_rms':rms(fold(default)), 'stock_mono_fold_rms':rms(fold(stock)),
            'stock_left_rms':rms(left(stock)), 'entropy_ranges_unchanged':True}
        wire = bytearray()
        for packet in packets: wire.extend(struct.pack('<H',len(packet)));wire.extend(packet)
        packet_path=args.output/f'{name}.opus-frames';packet_path.write_bytes(wire)
        write_pcm(args.output/f'{name}-stock.f32le',stock)
        if args.probe:
            modes={}
            for disabled in [False,True]:
                output=args.output/f'{name}-rust-{int(disabled)}.f32le'
                result=subprocess.run([str(args.probe),str(packet_path),str(output),str(int(disabled))],check=True,capture_output=True,timeout=20)
                assert json.loads(result.stdout) == ranges, 'Rust control changed entropy state'
                modes[str(int(disabled))]=compare(stock if disabled else default,read_pcm(output))
            metrics['rust']=modes
            if args.require_stock:
                assert modes['1']['fixed_gain_snr_db']>=35,metrics
        if args.probe_default:
            output=args.output/f'{name}-published-rust.f32le'
            subprocess.run([str(args.probe_default),str(packet_path),str(output),'0'],check=True,capture_output=True,timeout=20)
            pcm=read_pcm(output)
            metrics['published_rust_vs_stock']=compare(stock,pcm)
            metrics['published_rust_vs_default']=compare(default,pcm)
        if args.probe and bitrate == 48000:
            stock_reset, reset_ranges=reference.decode_control(packets,True,25)
            output=args.output/f'{name}-reset.f32le'
            result=subprocess.run([str(args.probe),str(packet_path),str(output),'1','25'],check=True,capture_output=True,timeout=20)
            assert json.loads(result.stdout)==reset_ranges
            metrics['rust_reset_retains_control']=compare(stock_reset,read_pcm(output))
            if args.require_stock: assert metrics['rust_reset_retains_control']['fixed_gain_snr_db']>=35
        cases.append(metrics)
    active=[case for case in cases if case['name'].startswith('intensity')]
    assert any(case['default_vs_stock']['fixed_gain_snr_db']<10 for case in active),'fixture did not exercise intensity inversion'
    assert any(case['stock_mono_fold_rms']>case['default_mono_fold_rms']*1.5 for case in active),'mono fold needs a real reference contrast'
    result={'mumble_source_revision':'35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0',
        'mumble_control':'AudioOutputSpeech.cpp85 OPUS_SET_PHASE_INVERSION_DISABLED(1)',
        'reference':str(args.opus_library),'cases':cases,
        'limits':['Fixed-gain component fixtures, no absolute perceptual guarantee.',
                  'Disabling inversion deliberately trades some stereo rendering for mono-fold behavior, matching Mumble.']}
    (args.output/'proof.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
