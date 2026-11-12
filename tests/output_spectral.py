#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Offline spectral audit of the actual native Mixer and device converter.

Requires NumPy/Matplotlib, explicit cached Rust probes and independent C libraries.
No capture, playback, network, profile or device API is opened. WAVs are synthetic.
"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

from output_quality import Reference
from output_resample import Speex

RATE = 48000
FRAME = 960
WINDOW = 960
HOP = 240
BANDS = ((0, 300), (300, 3400), (3400, 8000), (8000, 16000), (16000, 22050))


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def db(value):
    return float(20 * np.log10(max(float(value), 1e-15)))


def sources():
    """Deterministic independent LR material; fixed levels, never peak normalized."""
    t = np.arange(RATE * 3) / RATE
    active = (t > .15) & (t < 2.65)
    fade = np.minimum(np.clip((t - .15) / .03, 0, 1), np.clip((2.65 - t) / .03, 0, 1))
    left = sum(.045 * np.sin(2*np.pi*f*t + i*.37) for i, f in enumerate((127, 997, 3711, 7993, 13007, 17989)))
    right = sum(.04 * np.sin(2*np.pi*f*t + i*.53) for i, f in enumerate((211, 2003, 4999, 10007, 15101, 19001)))
    multiband = np.column_stack((left, right)) * fade[:, None]
    # Sharp, explicitly band-unlimited transients exercise attack/pre-echo and ringing.
    transient = np.zeros_like(multiband)
    for index, at in enumerate((.3, .71, 1.12, 1.73, 2.21)):
        start = round(at * RATE)
        tail = np.arange(4800) / RATE
        for channel in (0, 1):
            transient[start:start+4800, channel] += (.5 if channel == 0 else -.4) * np.exp(-tail*80) * np.cos(2*np.pi*(1300+channel*2700)*tail)
        transient[start, index % 2] += .18
    phase = np.cumsum(145 * (1 + .09*np.sin(2*np.pi*.65*t))) / RATE
    syllables = np.maximum(0, np.sin(2*np.pi*3.2*t)) ** 1.4
    speech = np.zeros_like(multiband)
    for harmonic in range(1, 29):
        weight = .07/harmonic + .015*np.exp(-((harmonic*145-900)/550)**2)
        speech[:, 0] += weight*np.sin(2*np.pi*harmonic*phase)
        speech[:, 1] += .85*weight*np.sin(2*np.pi*harmonic*phase + .2)
    speech *= (syllables*active)[:, None]
    # Independent stereo chords, decay, upper-band percussion and pitch glides.
    music = np.zeros_like(multiband)
    for beat in range(8):
        local = t - (.2 + .3*beat)
        envelope = np.exp(-np.maximum(local, 0)*8) * (local >= 0)
        for channel, chord in enumerate(((220, 277.183, 329.628), (261.626, 349.228, 440))):
            for fundamental in chord:
                for h in range(1, 10):
                    music[:, channel] += .028/h * envelope * np.sin(2*np.pi*fundamental*h*local)
            music[:, channel] += .025*envelope*np.sin(2*np.pi*(11500*local+3500*local*local+channel*.1))
    music *= fade[:, None]
    return (("multiband", multiband, 128000, 2049, .75),
            ("transients", transient, 128000, 2049, .75),
            ("speech-like", speech, 48000, 2049, .75),
            ("stereo-music", music, 128000, 2049, .75),
            ("overload-staging", multiband*3, 128000, 2049, 3.))


def encode(reference, source, bitrate, application):
    """Independent C encoder; return actual queried algorithmic lookahead."""
    lib = reference.lib
    error = ctypes.c_int()
    encoder = lib.opus_encoder_create(RATE, 2, application, ctypes.byref(error))
    assert encoder and error.value == 0
    try:
        for request, value in ((4002, bitrate), (4006, 0), (4022, 2)):
            assert lib.opus_encoder_ctl(encoder, request, value) == 0
        lookahead = ctypes.c_int()
        get = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_int))(("opus_encoder_ctl", lib))
        assert get(encoder, 4027, ctypes.byref(lookahead)) == 0
        packets = []
        for at in range(0, len(source), FRAME):
            pcm = np.ascontiguousarray(source[at:at+FRAME], dtype="<f4")
            output = ctypes.create_string_buffer(1275)
            count = lib.opus_encode_float(encoder, pcm.ctypes.data_as(ctypes.POINTER(ctypes.c_float)), FRAME, output, 1275)
            assert 0 < count <= 1024, count
            packets.append(output.raw[:count])
        return packets, lookahead.value
    finally:
        lib.opus_encoder_destroy(encoder)


def stft(pcm, rate):
    # Exact frame locations on both signals; no centering/padding/phase fitting.
    width = round(WINDOW*rate/RATE)
    hop = round(HOP*rate/RATE)
    window = np.hanning(width)
    frames = np.lib.stride_tricks.sliding_window_view(pcm, width, axis=0)[::hop]
    spectrum = np.fft.rfft(frames * window, axis=-1) * (2/window.sum())
    return spectrum, np.fft.rfftfreq(width, 1/rate), (np.arange(len(frames))*hop+width/2)/rate


def snr_pass(result, minimum):
    return result["exact_float_match"] or result["fixed_gain_snr_db"] >= minimum


def metrics(expected, actual, rate):
    assert expected.shape == actual.shape and expected.ndim == 2
    assert np.isfinite(expected).all() and np.isfinite(actual).all()
    error = actual.astype(float) - expected.astype(float)
    energy = np.sum(expected.astype(float)**2)
    noise = np.sum(error**2)
    ref, frequencies, _ = stft(expected, rate)
    residual, _, _ = stft(error, rate)
    observed, _, _ = stft(actual, rate)
    active = abs(ref) >= 10**(-70/20)
    magnitude_db = 20*np.log10(np.maximum(abs(observed), 1e-12)/np.maximum(abs(ref), 1e-12))
    band_results = []
    for low, high in BANDS:
        mask = (frequencies >= low) & (frequencies < min(high, rate/2))
        if not mask.any(): continue
        a, b = np.sum(abs(ref[..., mask])**2), np.sum(abs(residual[..., mask])**2)
        band_results.append({"hz": [low, min(high, rate/2)], "reference_energy": float(a),
            "residual_energy": float(b), "residual_relative_db": float(10*np.log10(max(b, 1e-30)/max(a, 1e-30)))})
    return {"exact_float_match": bool(noise == 0),
        "fixed_gain_snr_db": float(10*np.log10(energy/noise)) if noise > 0 else None,
        "max_abs_error": float(np.max(abs(error))), "rms_error_dbfs": db(np.sqrt(np.mean(error**2))),
        "reference_peak": float(np.max(abs(expected))), "actual_peak": float(np.max(abs(actual))),
        "actual_clipped_samples": int(np.sum(abs(actual) >= 1)),
        "active_bin_abs_magnitude_error_p95_db": float(np.percentile(abs(magnitude_db[active]), 95)) if active.any() else None,
        "bands": band_results}


def plot_pair(expected, actual, rate, path, title, labels=("C reference", "Actual native output")):
    fig, axes = plt.subplots(3, 2, figsize=(12, 8), sharex=True, sharey=True, constrained_layout=True)
    for row, signal in enumerate((expected, actual, actual-expected)):
        transform, frequencies, times = stft(signal, rate)
        for channel in (0, 1):
            levels = 20*np.log10(np.maximum(abs(transform[:, channel]), 1e-9))
            low, high = ((-100, 0) if row < 2 else (-160, -40))
            image = axes[row, channel].pcolormesh(times, frequencies/1000, levels.T,
                shading="nearest", vmin=low, vmax=high, cmap="magma")
            axes[row, channel].set_title((labels[row] if row < 2 else "Residual (separate fixed scale)") + (" · L" if channel == 0 else " · R"))
            axes[row, channel].set_ylim(0, min(rate/2000, 22.05))
            fig.colorbar(image, ax=axes[row, channel], label="dBFS amplitude/bin")
        axes[row, 0].set_ylabel("Frequency (kHz)")
    for ax in axes[-1]: ax.set_xlabel("Time (s)")
    fig.suptitle(title + "\nHann 20ms / hop 5ms · fixed gain · no fitted delay/warp")
    fig.savefig(path, dpi=130)
    plt.close(fig)


def write_wav(path, pcm, rate):
    # IEEE-float WAV preserves quiet residuals and finite headroom exactly; no
    # normalization, PCM16 quantization or extra clipping in evidence artifacts.
    payload = np.asarray(pcm, dtype="<f4").tobytes()
    assert np.isfinite(pcm).all() and len(payload) < 16 * 1024 * 1024
    fmt = struct.pack("<HHIIHH", 3, 2, rate, rate*8, 8, 32)
    path.write_bytes(b"RIFF" + struct.pack("<I", 36+len(payload)) + b"WAVEfmt "
        + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", len(payload)) + payload)


def convert(test_binary, source, rate, root, name, chunks, wide=False):
    before, after = root/(name+"-in.f32le"), root/(name+"-out.f32le")
    np.asarray(source, dtype="<f8" if wide else "<f4").tofile(before)
    env = os.environ | {"NUSCHL_OUTPUT_INPUT": str(before), "NUSCHL_OUTPUT_OUTPUT": str(after),
        "NUSCHL_OUTPUT_RATE": str(rate), "NUSCHL_OUTPUT_CHUNKS": chunks, "NUSCHL_OUTPUT_FORMAT": "f64le" if wide else "f32le"}
    subprocess.run([str(test_binary), "--ignored", "--exact", "live::output_fixture::dump"],
        env=env, check=True, capture_output=True, timeout=20)
    return np.fromfile(after, dtype="<f4").reshape(-1, 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mixer-probe", type=Path, required=True)
    parser.add_argument("--wide-mixer", action="store_true", help="f64 pre-SRC ring; probe --wide")
    parser.add_argument("--source-revision", default="caff07859c74cfc2a3601a47e4a133210ce8efca")
    parser.add_argument("--converter-test", type=Path, required=True)
    parser.add_argument("--converter-source", type=Path, required=True, help="Nix test derivation's immutable crate src")
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--speex-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parents[1]
    matched = {}
    for name in ("playback.rs", "output_fixture.rs"):
        current = repo/"apps/media-worker/src"/name
        cached = args.converter_source/"src"/name
        assert current.read_bytes() == cached.read_bytes(), "cached converter source mismatch: " + name
        matched[name] = sha(current)
    reference, speex = Reference(args.opus_library), Speex(args.speex_library)
    rows = []
    for name, source, bitrate, application, gain in sources():
        source = source.astype("<f4")
        root = args.output/name; root.mkdir(exist_ok=True)
        packets, lookahead = encode(reference, source, bitrate, application)
        decoded = np.asarray(reference.decode(packets, 2, phase_inversion_disabled=True), dtype=np.float32).reshape(-1, 2)
        # Fixed virtual-clock contract: ordered20ms packets start playing at tick2.
        ticks = 2*len(packets)+4
        events = []
        for tick in range(ticks):
            if tick % 2 == 0 and tick//2 < len(packets):
                index = tick//2
                events.append(f"P {tick} {tick*10000} {gain} {int(index == len(packets)-1)} {packets[index].hex()}")
            events.append(f"R {tick}")
        (root/"packets.events").write_text("\n".join(events)+"\n")
        # Also retain a compact wire-payload corpus independently of the probe grammar.
        (root/"packets.bin").write_bytes(b"".join(struct.pack(">H", len(p))+p for p in packets))
        subprocess.run([str(args.mixer_probe), str(root/"packets.events"), str(root/"mixer.pcm"), *(["--wide"] if args.wide_mixer else [])],
            capture_output=True, check=True, timeout=30)
        actual48 = np.fromfile(root/"mixer.pcm", dtype="<f8" if args.wide_mixer else "<f4").reshape(-1, 2)
        wide = np.zeros_like(actual48)
        wide[FRAME:FRAME+len(decoded)] = decoded.astype(float)*gain
        expected48 = np.clip(wide, -1, 1)
        rx = metrics(wide if args.wide_mixer else expected48, actual48, RATE)
        # Source↔codec loss is a DIFFERENT comparison, aligned only by C encoder's
        # queried lookahead. Its distortion is never attributed to RX implementation.
        lossy_source, lossy_decoded = source[:-lookahead], decoded[lookahead:]
        codec_loss = metrics(lossy_source, lossy_decoded, RATE)
        plot_pair(lossy_source, lossy_decoded, RATE, root/"codec-loss.png",
            f"{name}: C encoder/decoder loss · {bitrate//1000}kb/s · lookahead {lookahead} samples",
            ("Original synthetic input", "C libopus decoded (lossy)"))
        write_wav(root/"source.wav", source, RATE)
        write_wav(root/"aligned-source.wav", lossy_source, RATE)
        write_wav(root/"aligned-c-decoded.wav", lossy_decoded, RATE)
        rate_rows = []
        for rate in (48000, 44100):
            actual = convert(args.converter_test, actual48, rate, root, f"{rate}-whole", "8192", args.wide_mixer)
            chunked = convert(args.converter_test, actual48, rate, root, f"{rate}-irregular", "1,127,511,37,960,256", args.wide_mixer)
            assert np.array_equal(actual, chunked), (name, rate, "callback-boundary PCM mismatch")
            # Isolate SRC from decoder: give the C SRC exactly the same actual Mixer PCM.
            src_reference, latency = speex.convert(actual48.reshape(-1).tolist(), rate)
            src_reference = np.asarray(src_reference, dtype=np.float32).reshape(-1, 2)[:len(actual)]
            src_reference = np.clip(src_reference, -1, 1)
            src_metrics = metrics(src_reference, actual, rate)
            # Source-matched component reference: same mix-before-SRC clip as Nuschl.
            expected, _ = speex.convert((wide if args.wide_mixer else expected48).reshape(-1).tolist(), rate)
            expected = np.clip(np.asarray(expected, dtype=np.float32).reshape(-1, 2)[:len(actual)], -1, 1)
            pipeline_metrics = metrics(expected, actual, rate)
            # A separate stock-order diagnostic: resample wide audio BEFORE final clip.
            stock, _ = speex.convert(wide.reshape(-1).tolist(), rate)
            stock = np.clip(np.asarray(stock, dtype=np.float32).reshape(-1, 2)[:len(actual)], -1, 1)
            stock_metrics = metrics(stock, actual, rate)
            plot_pair(stock, actual, rate, root/f"{rate}-stock-order.png",
                f"{name}: native RX vs C components in stock Mumble order · {rate}Hz")
            plot_pair(expected, actual, rate, root/f"{rate}-matched-order.png",
                f"{name}: identical packets and clip/SRC order · {rate}Hz")
            write_wav(root/f"{rate}-reference.wav", stock, rate)
            write_wav(root/f"{rate}-actual.wav", actual, rate)
            write_wav(root/f"{rate}-residual.wav", actual-stock, rate)
            rate_rows.append({"rate": rate, "src_delay_output_frames": latency, "chunks_bit_exact": True,
                "isolated_device_src": src_metrics, "matched_order_pipeline": pipeline_metrics,
                "stock_clip_order_diagnostic": stock_metrics,
                "component_gate_passed": snr_pass(rx, 35) and snr_pass(src_metrics, 100),
                "stock_order_gate_passed": snr_pass(stock_metrics, 35)})
        rows.append({"name": name, "synthetic": True, "channels": 2, "bitrate": bitrate, "application": application,
            "gain": gain, "packets": len(packets), "max_packet_bytes": max(map(len, packets)),
            "lookahead_samples": lookahead, "source_peak": float(np.max(abs(source))), "mixer_vs_c_decoder": rx,
            "codec_loss_vs_input": codec_loss, "rates": rate_rows})
        print(name, "mixer SNR", round(rx["fixed_gain_snr_db"], 2) if not rx["exact_float_match"] else "exact", "stock order SNR", [round(r["stock_clip_order_diagnostic"]["fixed_gain_snr_db"], 2) if not r["stock_clip_order_diagnostic"]["exact_float_match"] else "exact" for r in rate_rows], flush=True)
    artifacts = {str(p.relative_to(args.output)): {"sha256": sha(p), "bytes": p.stat().st_size}
        for p in sorted(args.output.rglob("*")) if p.is_file() and p.suffix in (".wav", ".png", ".bin")}
    report = {"format": 1, "source_base": args.source_revision, "wide_pre_src_ring": args.wide_mixer,
        "host_devices": False, "network": False, "profile": False, "gain_fitted": False, "delay_fitted": False,
        "alignment": "Fixed20ms virtual jitter start; both SRCs retain their real filter latency. Input/codec-loss only: independently queried C encoder lookahead. No correlation/warp/gain fitting.",
        "reference_kind": "Independent C libopus1.6.1 and SpeexDSP1.2.1 quality3 components, not an executed stock Mumble client",
        "phase_inversion_disabled": True, "stft": {"window_ms": 20, "hop_ms": 5, "window": "Hann", "scale": "2/sum(window), amplitude dBFS/bin", "active_bins_above_dbfs": -70},
        "binary_inputs": {key: {"path": str(getattr(args, key)), "sha256": sha(getattr(args, key))}
            for key in ("mixer_probe", "converter_test", "opus_library", "speex_library")},
        "cached_converter_source": str(args.converter_source), "matched_production_sources": matched,
        "current_source_hashes": {name: sha(repo/name) for name in ("apps/media-worker/src/mixer.rs", "apps/media-worker/src/jitter.rs", "crates/audio-jitter/src/lib.rs", "vendor/opus-pure/src/decoder.rs", "vendor/speexdsp-resampler/src/speex.rs")},
        "cases": rows, "artifacts": artifacts,
        "all_component_gates_passed": all(r["component_gate_passed"] for row in rows for r in row["rates"]),
        "all_stock_order_gates_passed": all(r["stock_order_gate_passed"] for row in rows for r in row["rates"]),
        "limits": ["No native encoder/input/DSP/capture/network exercised", "No hardware/listening/perceptual parity established", "WAVs retain IEEEfloat32 samples without normalization/clipping; float PCM drives all metrics", "Overload clip/SRC ordering is a separately reported existing gap; lossy source/codec error is not an RX defect"]}
    (args.output/"proof.json").write_text(json.dumps(report, indent=2)+"\n")
    if not report["all_component_gates_passed"] or not report["all_stock_order_gates_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
