// SPDX-License-Identifier: EUPL-1.2
// This module is a Vite-emitted AudioWorklet asset: keep it self-contained.
const FRAME_SAMPLES = 960;
const CAPACITY = FRAME_SAMPLES * 10; // 200 ms, per channel at 48 kHz.
const PREROLL = FRAME_SAMPLES * 3; // 60 ms, also used after an underrun.

class BrowserAudioProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.samples = new Float32Array(CAPACITY * 2);
    this.read = 0;
    this.write = 0;
    this.available = 0;
    this.started = false;
    this.playing = false;
    this.epoch = 0;
    this.port.onmessage = ({ data }) => {
      if (data.reset) {
        this.read = 0;
        this.write = 0;
        this.available = 0;
        this.started = false;
        this.epoch = data.epoch;
        this.report(false);
      }
      if (data.epoch === this.epoch) {
        for (const frame of data.frames) this.append(frame);
      }
      // Acknowledge receipt, not playout. The ring bounds retained PCM; this
      // handshake separately prevents an unbounded MessagePort backlog.
      this.port.postMessage({ type: "ack", id: data.id });
    };
  }

  append(frame) {
    const overflow = this.available + FRAME_SAMPLES - CAPACITY;
    if (overflow > 0) {
      this.read = (this.read + overflow) % CAPACITY;
      this.available -= overflow;
    }
    // Both the ring capacity and incoming records are whole 960-sample
    // frames, so writes never straddle its end. Reads use render quanta.
    this.samples.set(frame, this.write * 2);
    this.write = (this.write + FRAME_SAMPLES) % CAPACITY;
    this.available += FRAME_SAMPLES;
  }

  report(playing) {
    if (this.playing === playing) return;
    this.playing = playing;
    this.port.postMessage({ type: "state", epoch: this.epoch, playing });
  }

  process(_inputs, outputs) {
    const output = outputs[0];
    for (const channel of output) channel.fill(0);
    if (!this.started) {
      if (this.available < PREROLL) return true;
      this.started = true;
    }
    const left = output[0];
    const right = output[1];
    const count = Math.min(left.length, this.available);
    for (let i = 0; i < count; i++) {
      left[i] = this.samples[this.read * 2];
      right[i] = this.samples[this.read * 2 + 1];
      this.read = (this.read + 1) % CAPACITY;
    }
    this.available -= count;
    // Only announce playing once decoded samples actually reach the graph.
    this.report(count > 0);
    if (this.available === 0) {
      this.started = false;
      this.report(false);
    }
    return true;
  }
}

registerProcessor("nuschl-browser-audio", BrowserAudioProcessor);
