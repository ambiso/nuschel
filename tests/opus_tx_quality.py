#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Transmit-only spectral audit. Synthetic PCM + test-side C libopus; no devices.

Run with cached NumPy/Matplotlib. No gain fitting, audio normalization, network,
personal profile, or objective-to-perceptual equivalence assertion.
"""
import argparse
import collections
import ctypes
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import time

import numpy as np
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

from mumble_client import ReferenceOpus

RATE, FRAME = 48000, 960
PROFILES = ["native-voip", "native-audio", "native-lowdelay", "browser-voip", "browser-audio", "browser-lowdelay", "mumble"]


def fixture(name, seconds=4):
    count = round(seconds * RATE)
    t = np.arange(count) / RATE
    rng = np.random.default_rng(0x4E555343)
    noise = rng.normal(0, 1, count)
    # Test input is explicitly synthesized, not an intelligible-speech corpus.
    if name == "speech-like":
        phase = np.cumsum(112 + 35 * np.sin(2 * np.pi * .7 * t)) * 2 * np.pi / RATE
        data = np.zeros(count)
        fundamental = 112 + 35 * np.sin(2 * np.pi * .7 * t)
        for harmonic in range(1, 65):
            hz = fundamental * harmonic
            weight = sum(amplitude * np.exp(-.5 * ((hz - center) / width)**2)
                         for center, width, amplitude in [(600, 130, 1), (1350, 210, .8), (2800, 350, .4)])
            data += np.sin(phase * harmonic) * weight / harmonic
        bins = np.fft.rfft(noise)
        frequencies = np.fft.rfftfreq(count, 1 / RATE)
        bins *= (frequencies >= 4500) & (frequencies < 15000)
        fricative = np.fft.irfft(bins, count)
        syllables = .35 + .65 * np.maximum(0, np.sin(2 * np.pi * 3.7 * t))
        data = .4 * data * syllables + .045 * fricative * ((t % .9) > .62)
    elif name == "music":
        data = np.zeros(count)
        for frequency in [220, 277.182631, 329.627557]:
            for harmonic in range(1, 25):
                data += .08 / harmonic * np.sin(2 * np.pi * frequency * harmonic * t + harmonic * .37)
        data *= .65 + .35 * np.sin(2 * np.pi * 1.8 * t)**2
        data += .16 * noise * np.exp(-55 * (t % .4))
    elif name == "transients":
        phase = t % .4
        data = .6 * np.exp(-phase * 180) * np.sin(2 * np.pi * (1200 * phase + 65000 * phase**2))
        data += .17 * noise * np.exp(-phase * 250)
    elif name == "highband":
        data = np.zeros(count)
        for index, frequency in enumerate([997, 6001, 10007, 14009, 17011, 19001]):
            start = .2 + index * .55
            region = (t >= start) & (t < start + .45)
            local = t[region] - start
            envelope = np.minimum(1, local / .015) * np.minimum(1, (.45 - local) / .015)
            data[region] = .25 * envelope * np.sin(2 * np.pi * frequency * local)
    else:
        raise ValueError(name)
    data[(t < .2) | (t >= seconds - .2)] = 0
    assert np.max(np.abs(data)) < 1
    # A common16bit-representable source: C Mumble policy uses s16; matched
    # native/browser comparisons still explicitly use float24 APIs on both sides.
    return (np.trunc(data * 32768).astype(np.int16).astype(np.float32) / 32768).astype("<f4")


def controls(profile, bitrate):
    application = 2051 if profile.endswith("lowdelay") else 2049 if profile.endswith("audio") else 2048
    if profile == "mumble":
        application = 2051 if bitrate >= 64000 else 2049 if bitrate >= 32000 else 2048
    return {"application": application, "complexity": 5 if profile.startswith("browser-") else 9,
            "vbr": int(profile != "mumble"), "vbr_constraint": 1,
            "fec": int(profile.startswith("native-")), "loss_hint": 5 if profile.startswith("native-") else 0,
            "precision": 16 if profile == "mumble" else 24, "maximum_packet_bytes": min(bitrate // 400, 960)}


class CReference:
    def __init__(self, library):
        self.lib = ReferenceOpus(library).lib
        self.set_ctl = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_int, ctypes.c_int)(("opus_encoder_ctl", self.lib))
        self.query_ctl = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_int))(("opus_encoder_ctl", self.lib))
        self.lib.opus_encode.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int16), ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
        self.lib.opus_get_version_string.restype = ctypes.c_char_p
        self.version = self.lib.opus_get_version_string().decode()

    def encode(self, source, settings, bitrate):
        error = ctypes.c_int()
        encoder = self.lib.opus_encoder_create(RATE, 1, settings["application"], ctypes.byref(error))
        assert encoder and error.value == 0
        try:
            for request, value in [(4002, bitrate), (4010, settings["complexity"]), (4006, settings["vbr"]),
                                   (4020, settings["vbr_constraint"]), (4012, settings["fec"]), (4014, settings["loss_hint"])]:
                assert self.set_ctl(encoder, request, value) == 0
            delay = ctypes.c_int()
            assert self.query_ctl(encoder, 4027, ctypes.byref(delay)) == 0
            packets, costs = [], []
            for frame in source.reshape(-1, FRAME):
                packet = ctypes.create_string_buffer(settings["maximum_packet_bytes"])
                if settings["precision"] == 16:
                    pcm = np.trunc(frame * 32768).astype("<i2")
                    begin = time.perf_counter_ns()
                    count = self.lib.opus_encode(encoder, pcm.ctypes.data_as(ctypes.POINTER(ctypes.c_int16)), FRAME, packet, len(packet))
                else:
                    begin = time.perf_counter_ns()
                    count = self.lib.opus_encode_float(encoder, frame.ctypes.data_as(ctypes.POINTER(ctypes.c_float)), FRAME, packet, len(packet))
                costs.append(time.perf_counter_ns() - begin)
                assert count > 0, count
                packets.append(packet.raw[:count])
            return packets, {"lookahead_samples": delay.value, "encode_p50_ns": float(np.percentile(costs, 50)),
                             "encode_p95_ns": float(np.percentile(costs, 95)), "encode_max_ns": max(costs)}
        finally:
            self.lib.opus_encoder_destroy(encoder)

    def decode(self, packets, loss="none", fec=False):
        error = ctypes.c_int()
        decoder = self.lib.opus_decoder_create(RATE, 1, ctypes.byref(error))
        assert decoder and error.value == 0
        missing = set()
        if loss == "isolated":
            missing = set(range(17, len(packets) - 1, 13))
        elif loss == "burst":
            missing = {index for at in range(31, len(packets) - 2, 50) for index in (at, at+1)}
        try:
            result = []
            for index, packet in enumerate(packets):
                recovery = index in missing and fec and index+1 not in missing and index+1 < len(packets)
                payload = packets[index+1] if recovery else None if index in missing else packet
                output = (ctypes.c_float * FRAME)()
                count = self.lib.opus_decode_float(decoder, payload, len(payload) if payload else 0, output, FRAME, int(recovery))
                assert count == FRAME, count
                result.append(np.ctypeslib.as_array(output).copy())
            return np.concatenate(result), sorted(missing)
        finally:
            self.lib.opus_decoder_destroy(decoder)


def read_packets(path):
    raw = path.read_bytes()
    packets, at = [], 0
    while at < len(raw):
        length = struct.unpack_from("<H", raw, at)[0]
        at += 2
        assert 0 < length <= 960 and at + length <= len(raw)
        packets.append(raw[at:at+length]);at += length
    return packets


def spectrogram(data):
    window = np.hanning(1024)
    frames = np.lib.stride_tricks.sliding_window_view(data, 1024)[::256]
    spectrum = np.abs(np.fft.rfft(frames * window, axis=1)) * 2 / window.sum()
    return spectrum, np.fft.rfftfreq(1024, 1 / RATE)


def metrics(source, decoded, delay):
    expected, observed = source[:len(decoded)-delay], decoded[delay:]
    energy = np.dot(expected.astype(np.float64), expected)
    residual = observed.astype(np.float64) - expected
    a, frequencies = spectrogram(expected)
    b, _ = spectrogram(observed)
    bands = {}
    for low, high in [(80,1000), (1000,4000), (4000,8000), (8000,12000), (12000,16000), (16000,20000)]:
        mask = (frequencies >= low) & (frequencies < high)
        ea, eb = np.mean(a[:,mask]**2), np.mean(b[:,mask]**2)
        bands[f"{low}-{high}"] = {"source_energy_db": float(10*np.log10(max(ea,1e-20))),
                                  "level_error_db": float(10*np.log10(max(eb,1e-20)/max(ea,1e-20)))}
    return {"samples": len(observed), "alignment": "declared lookahead only; no gain/phase/lag fitting",
            "fixed_gain_snr_db": float(10*np.log10(max(energy,1e-30)/max(np.dot(residual,residual),1e-30))),
            "rms_gain_db": float(10*np.log10(max(np.dot(observed.astype(np.float64),observed),1e-30)/max(energy,1e-30))),
            "peak": float(np.max(np.abs(observed))), "bands": bands}


def float_wav(path, samples):
    data = samples.astype("<f4").tobytes()
    # IEEE-float WAV preserves decoded headroom, with no normalization/clipping.
    fmt = struct.pack("<HHIIHH",3,1,RATE,RATE*4,4,32)
    path.write_bytes(b"RIFF"+struct.pack("<I",36+len(data))+b"WAVEfmt "+struct.pack("<I",16)+fmt+b"data"+struct.pack("<I",len(data))+data)


def render_plots(inputs, output, profiles, bitrates):
    for name in ["speech-like","music","transients","highband"]:
        for bitrate in [rate for rate in bitrates if rate in [64000,128000]]:
            source=np.fromfile(inputs/(name+".f32le"),dtype="<f4")
            panels=[("Source (no normalization)",source)]
            for profile in [p for p in PROFILES if p!="mumble" and p in profiles]:
                delay=120 if profile.endswith("lowdelay") else 312
                decoded=np.fromfile(inputs/f"{name}-{bitrate}-{profile}-rust.f32le",dtype="<f4")
                panels.append((profile+" (Rust encoder)",decoded[delay:]))
            if "mumble" in profiles:
                decoded=np.fromfile(inputs/f"{name}-{bitrate}-mumble-libopus.f32le",dtype="<f4")
                panels.append(("Mumble source policy (C encoder, s16/CBR; no capture preprocessing)",decoded[120:]))
            fig,axes=plt.subplots(len(panels),1,figsize=(12,2.1*len(panels)),layout="constrained",sharex=True)
            for axis,(label,pcm) in zip(axes,panels):
                spectrum,_=spectrogram(pcm);image=axis.imshow(20*np.log10(np.maximum(spectrum.T,1e-7)),origin="lower",aspect="auto",extent=[0,len(pcm)/RATE,0,24],vmin=-100,vmax=0,cmap="magma")
                axis.set_ylim(0,21);axis.set_ylabel("kHz");axis.set_title(label,loc="left",fontsize=10)
            axes[-1].set_xlabel("seconds, aligned only by encoder-reported delay")
            fig.colorbar(image,ax=axes,label="dBFS (identical scale)");fig.suptitle(f"TX {name}: {bitrate/1000:g} kbit/s; all packets decoded by independent libopus")
            fig.savefig(output/f"{name}-{bitrate}-spectrogram.png",dpi=120);plt.close(fig)


    required={"native-voip","native-audio","browser-voip","browser-audio","mumble"}
    if {64000,128000}.issubset(bitrates) and required.issubset(profiles):
        fig,axes=plt.subplots(6,2,figsize=(14,12),layout="constrained",sharex=True,sharey=True)
        for column,(name,bitrate) in enumerate([("speech-like",64000),("music",128000)]):
            panels=[("Source",np.fromfile(inputs/(name+".f32le"),dtype="<f4"))]
            for profile in ["native-voip","native-audio","browser-voip","browser-audio"]:
                data=np.fromfile(inputs/f"{name}-{bitrate}-{profile}-rust.f32le",dtype="<f4")[312:]
                panels.append((profile+" (Rust)",data))
            panels.append(("Mumble policy (C / CBR / s16)",np.fromfile(inputs/f"{name}-{bitrate}-mumble-libopus.f32le",dtype="<f4")[120:]))
            for row,(label,pcm) in enumerate(panels):
                spectrum,_=spectrogram(pcm);image=axes[row,column].imshow(20*np.log10(np.maximum(spectrum.T,1e-7)),origin="lower",aspect="auto",extent=[0,len(pcm)/RATE,0,24],vmin=-100,vmax=0,cmap="magma")
                axes[row,column].set_ylim(0,21);axes[row,column].set_title(label,loc="left",fontsize=10)
                if column==0:axes[row,column].set_ylabel("kHz")
            axes[0,column].set_title(f"Source: {name}, {bitrate/1000:g} kbit/s",loc="left",fontsize=10)
            axes[-1,column].set_xlabel("seconds (declared codec delay removed)")
        fig.colorbar(image,ax=axes,label="dBFS; same scale, no gain normalization")
        fig.suptitle("Synthetic TX codec comparison: all packets independently decoded by C libopus\nEncoder-only evidence; capture processing and perceptual equivalence are not established",fontsize=12)
        fig.savefig(output/"summary.png",dpi=110);plt.close(fig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--opus-library", type=Path)
    parser.add_argument("--replot", type=Path, help="read frozen PCM artifacts without re-encoding or altering their proof")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--bitrates", nargs="+", type=int, default=[24000,48000,64000,128000])
    parser.add_argument("--profiles", nargs="+", choices=PROFILES, default=PROFILES)
    args = parser.parse_args();args.output.mkdir(parents=True, exist_ok=True)
    if args.replot:
        render_plots(args.replot,args.output,args.profiles,args.bitrates);return
    if not args.probe or not args.opus_library:parser.error("--probe and --opus-library are required for encoding")
    reference = CReference(args.opus_library)
    results = []
    for name in ["speech-like","music","transients","highband"]:
        source = fixture(name);path=args.output/(name+".f32le");source.tofile(path);float_wav(args.output/(name+"-source.wav"),source)
        for bitrate in args.bitrates:
            for profile in args.profiles:
                settings = controls(profile,bitrate);label=f"{name}-{bitrate}-{profile}"
                packet_path=args.output/(label+".opus-frames")
                pure = subprocess.run([str(args.probe),profile,str(bitrate),str(path),str(packet_path),str(settings["precision"])],check=True,capture_output=True,text=True,timeout=30)
                packets = read_packets(packet_path);pure_stats=json.loads(pure.stdout)
                c_packets,c_stats=reference.encode(source,settings,bitrate)
                assert pure_stats["lookahead_samples"] == c_stats["lookahead_samples"]
                for engine,encoded,timing in [("rust",packets,pure_stats),("libopus",c_packets,c_stats)]:
                    modes=collections.Counter("CELT" if p[0]>>3>=16 else "HYBRID" if p[0]>>3>=12 else "SILK" for p in encoded)
                    configs=collections.Counter(str(p[0]>>3) for p in encoded)
                    for loss in ["none","isolated","burst"]:
                        decoded,missing=reference.decode(encoded,loss,bool(settings["fec"]))
                        row={"fixture":name,"bitrate":bitrate,"profile":profile,"engine":engine,"settings":settings,
                             "loss":loss,"missing_packets":missing,"toc_modes":dict(modes),"toc_configs":dict(configs),
                             "actual_payload_bps":sum(map(len,encoded))*8/(len(encoded)*.02),"timing":timing,
                             "metrics":metrics(source,decoded,timing["lookahead_samples"])}
                        results.append(row)
                        if loss=="none":
                            decoded.tofile(args.output/(label+"-"+engine+".f32le"))
                            if bitrate in [64000,128000] and engine=="rust":
                                float_wav(args.output/(label+"-"+engine+".wav"),decoded)
        print(json.dumps({"fixture":name,"cases_so_far":len(results)}),flush=True)
    render_plots(args.output,args.output,args.profiles,args.bitrates)
    report={"completed":True,"reference_version":reference.version,"reference_library":str(args.opus_library),
            "reference_sha256":hashlib.sha256(args.opus_library.read_bytes()).hexdigest(),"source":"owned deterministic synthetic signals, seed0x4E555343; no speech corpus/hardware/network",
            "gain_fitted":False,"delay_fitted":False,"results":results,
            "limits":["Standalone encoder controls reflect source; production live/WASM activation still needs separate proof.","No listening/perceptual-equivalence guarantee from these synthetic metrics.","Policy candidates preserve current native/browser complexity/FEC/CVBR; only application changes."]}
    (args.output/"proof.json").write_text(json.dumps(report,indent=2)+"\n")


if __name__ == "__main__":
    main()
