#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual native codec IPC and shipped WASM TX versus independent C decode.

No capture/device APIs: this isolates the production encoder constructors from
preprocessing, consent and transport. Those remain separate private-live gates.
"""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import subprocess

import numpy as np

from opus_eval import Worker
from opus_tx_quality import CReference, fixture, controls, metrics, read_packets, float_wav


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--native-worker", required=True, type=Path)
    parser.add_argument("--wasm-codec", required=True, type=Path)
    parser.add_argument("--opus-library", required=True, type=Path)
    parser.add_argument("--probe", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--require-control-snr-db", type=float, help="optional separate cross-target equality gate; not a perceptual-quality contract")
    args = parser.parse_args();args.output.mkdir(parents=True, exist_ok=True)
    reference=CReference(args.opus_library);results=[]
    for name in ["speech-like","music","transients","highband"]:
        source=fixture(name);path=args.output/(name+".f32le");source.tofile(path)
        worker=Worker(args.native_worker)
        try:
            packets=[bytes(packet) for packet in worker.encode(source.tolist())]
        finally:
            worker.close()
        assert len(packets)==len(source)//960
        decoded,_=reference.decode(packets)
        assert np.isfinite(decoded).all()
        settings=controls("native-audio",128000)
        settings.update(fec=0,loss_hint=0,maximum_packet_bytes=1275)
        c_packets,info=reference.encode(source,settings,128000)
        c_decoded,_=reference.decode(c_packets)
        results.append({"path":"actual native codec IPC","fixture":name,"bitrate":128000,
                        "native_binary":str(args.native_worker),"settings":settings,
                        "lookahead_samples":info["lookahead_samples"],"lookahead_scope":"C query +actual native factory unit assertion, not an IPC telemetry field",
                        "metrics":metrics(source,decoded,info["lookahead_samples"]),"matched_C_metrics":metrics(source,c_decoded,info["lookahead_samples"]),
                        "toc_configs":dict(collections.Counter(str(p[0]>>3) for p in packets))})
        decoded.tofile(args.output/(name+"-native.f32le"));float_wav(args.output/(name+"-native.wav"),decoded)
        for bitrate in [32000,48000,64000,128000]:
            packet_path=args.output/f"{name}-wasm-{bitrate}.opus-frames"
            run=subprocess.run(["node",str(Path(__file__).with_name("opus_tx_wasm.mjs")),str(args.wasm_codec),str(path),str(packet_path),str(bitrate)],check=True,capture_output=True,text=True,timeout=30)
            metadata=json.loads(run.stdout);assert metadata["lookahead_samples"]==312
            packets=read_packets(packet_path);assert len(packets)==len(source)//960
            assert all(len(packet)<=bitrate//400 for packet in packets)
            decoded,_=reference.decode(packets)
            assert np.isfinite(decoded).all()
            settings=controls("browser-audio",bitrate);c_packets,c_info=reference.encode(source,settings,bitrate)
            assert metadata["lookahead_samples"]==c_info["lookahead_samples"]
            c_decoded,_=reference.decode(c_packets)
            # Exact source-derived browser Audio controls on the standalone
            # native Rust build, decoded by independent C on BOTH sides. This
            # checks the production WASM factory/configuration, not independent
            # codec implementation quality (the C-encoder matrix covers that).
            expected_path=args.output/f"{name}-native-browser-control-{bitrate}.opus-frames"
            subprocess.run([str(args.probe),"browser-audio",str(bitrate),str(path),str(expected_path),"24"],check=True,capture_output=True,timeout=30)
            expected,_=reference.decode(read_packets(expected_path))
            difference=decoded.astype(np.float64)-expected
            equivalence=float(10*np.log10(max(np.dot(expected.astype(np.float64),expected),1e-30)/max(np.dot(difference,difference),1e-30)))
            row={"path":"actual wasm-bindgen VoiceEncoder","fixture":name,"bitrate":bitrate,"settings":settings,"metadata":metadata,
                 "metrics":metrics(source,decoded,metadata["lookahead_samples"]),"matched_C_metrics":metrics(source,c_decoded,c_info["lookahead_samples"]),
                 "standalone_controls_decode_snr_db":equivalence,"toc_configs":dict(collections.Counter(str(p[0]>>3) for p in packets))}
            row["cross_target_60db_diagnostic_passed"]=equivalence>=60
            if args.require_control_snr_db is not None:
                assert equivalence>=args.require_control_snr_db,row
            results.append(row);decoded.tofile(args.output/f"{name}-wasm-{bitrate}.f32le")
        print(json.dumps({"fixture":name,"passed_paths":len(results)}),flush=True)
    report={"passed":True,"passed_scope":"Audio constructor/packet/delay/source validation; not cross-target equality or perceptual parity",
            "cross_target_60db_diagnostic_passed":all(row.get("cross_target_60db_diagnostic_passed",True) for row in results),"source":"production native Audio factories +actual wasm-bindgen artifact; synthetic PCM only",
            "native_worker":str(args.native_worker),"wasm_codec":str(args.wasm_codec),
            "wasm_sha256":hashlib.sha256((args.wasm_codec/"nuschl_audio_wasm_bg.wasm").read_bytes()).hexdigest(),
            "reference_version":reference.version,"reference_sha256":hashlib.sha256(args.opus_library.read_bytes()).hexdigest(),
            "results":results,"limits":["No capture preprocessing/transport/browser/device proof in this constructor-specific fixture.",
            "Native IPC always requests128k; live factory budget/reset tests cover forced32/48/64k without startup/capture changes.",
            "WASM/native Rust output equality is a separate optional diagnostic, currently failing; matched C source metrics remain separate. No perceptual-quality guarantee."]}
    (args.output/"proof.json").write_text(json.dumps(report,indent=2)+"\n")


if __name__=="__main__":
    main()
