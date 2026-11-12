// SPDX-License-Identifier: EUPL-1.2
// Static same-origin AudioWorklet module. No imports, network or device handles.
export const AUDIO_RATE = 48000;
export const AUDIO_FRAME = 960;
export const MAX_SPEAKERS = 16;
// Frames per channel: 200 ms leaves room for a legal 120 ms packet plus
// the 20 ms startup hold without discarding its tail at the next arrival.
export const MAX_QUEUED_SAMPLES = 9600;

// Shared with the real processor tests: finite, bounded FIFO per speaker. An
// overflow discards the oldest queued samples, never accumulating stale audio.
export class PcmMixer {
  constructor() { this.epoch = ""; this.enabled = false; this.speakers = new Map(); }
  configure(epoch, enabled) {
    this.epoch = typeof epoch === "string" && epoch.length <= 200 ? epoch : "";
    this.enabled = Boolean(enabled && this.epoch);
    this.speakers.clear();
  }
  enqueue({epoch, speaker, samples, volume = 1, channels = 1, frames = samples?.length / channels}) {
    if (!this.enabled || epoch !== this.epoch || !Number.isInteger(speaker) || speaker < 0 || speaker > 0xffffffff ||
        !(samples instanceof Float32Array) || ![1,2].includes(channels) || !Number.isInteger(frames) ||
        frames < 1 || frames > MAX_QUEUED_SAMPLES || samples.length !== frames * channels ||
        !Number.isFinite(volume) || volume < 0 || volume > 4) return false;
    if (samples.some(value => !Number.isFinite(value))) return false;
    let state = this.speakers.get(speaker);
    if (!state) {
      if (this.speakers.size >= MAX_SPEAKERS) return false;
      // Size/read/hold are FRAMES, storage is interleaved LR; 200 ms cap.
      state = {data:new Float32Array(MAX_QUEUED_SAMPLES * 2), read:0, size:0, idle:0, hold:AUDIO_FRAME};
      this.speakers.set(speaker,state);
    }
    const overflow = Math.max(0,state.size + frames - MAX_QUEUED_SAMPLES);
    state.read = (state.read + overflow) % MAX_QUEUED_SAMPLES;
    state.size -= overflow;
    for (let i=0;i<frames;i++) {
      const at=((state.read + state.size + i) % MAX_QUEUED_SAMPLES) * 2;
      for(let channel=0;channel<2;channel++) {
        // Gain before full-scale protection: finite codec overshoot can safely
        // become quiet audio. Do not clip it irreversibly before attenuation.
        state.data[at+channel] = Math.max(-1,Math.min(1,samples[i*channels+(channels===2?channel:0)] * volume));
      }
    }
    state.size += frames; state.idle = 0;
    return true;
  }
  remove(speaker) { this.speakers.delete(speaker); }
  render(left,right=null) {
    left.fill(0);right?.fill(0);
    if (!this.enabled) return;
    for (const [speaker,state] of this.speakers) {
      const offset=Math.min(left.length,state.hold);state.hold-=offset;
      const length = Math.min(left.length-offset,state.size);
      for(let i=0;i<length;i++) {
        const at=((state.read+i)%MAX_QUEUED_SAMPLES)*2;
        if(right){left[offset+i]+=state.data[at];right[offset+i]+=state.data[at+1];}
        else left[offset+i]+=(state.data[at]+state.data[at+1])*.5;
      }
      state.read=(state.read+length)%MAX_QUEUED_SAMPLES;state.size-=length;
      state.idle=state.size===0?state.idle+left.length:0;
      if(state.idle>AUDIO_RATE) this.speakers.delete(speaker);
    }
    for(let i=0;i<left.length;i++) {
      left[i]=Math.max(-1,Math.min(1,left[i]));
      if(right)right[i]=Math.max(-1,Math.min(1,right[i]));
    }
  }
}

// Explicit credits ensure the browser's MessagePort cannot build an unbounded
// capture backlog while the codec worker is busy, suspended or terminated.
export class CaptureFrames {
  constructor(send) { this.send=send;this.buffer=new Float32Array(AUDIO_FRAME);this.configure("",false); }
  configure(epoch,enabled) {
    this.epoch=typeof epoch==="string"&&epoch.length<=200?epoch:"";
    this.enabled=Boolean(enabled&&this.epoch);this.cursor=0;this.frame=0;this.credits=this.enabled?2:0;
    this.buffer.fill(0);
  }
  credit(epoch) { if(epoch===this.epoch&&this.enabled)this.credits=Math.min(2,this.credits+1); }
  append(input) {
    if(!this.enabled)return;
    for(let i=0;i<input.length;i++) {
      const value=input[i];this.buffer[this.cursor++]=Number.isFinite(value)?Math.max(-1,Math.min(1,value)):0;
      if(this.cursor===AUDIO_FRAME) {
        if(this.credits>0) {
          this.credits--;
          this.send(this.epoch,this.buffer.slice(),this.frame);
        }
        this.cursor=0;this.frame++;
      }
    }
  }
}

if(typeof registerProcessor==="function") {
  class CaptureProcessor extends AudioWorkletProcessor {
    constructor() {
      super();
      this.capture=new CaptureFrames((epoch,samples,frame)=>this.port.postMessage({kind:"capture",epoch,samples,frame},[samples.buffer]));
      this.port.onmessage=({data})=>{
        if(data?.kind==="configure")this.capture.configure(data.epoch,data.enabled);
        else if(data?.kind==="credit")this.capture.credit(data.epoch);
      };
    }
    process(inputs,outputs) {
      // Capture must never be monitored locally or enter the playback reference.
      for(const bus of outputs)for(const channel of bus)channel.fill(0);
      const mono=inputs[0]?.[0];
      if(mono&&sampleRate===AUDIO_RATE)this.capture.append(mono);
      return true;
    }
  }
  class PlaybackProcessor extends AudioWorkletProcessor {
    constructor() {
      super();this.mixer=new PcmMixer();
      this.port.onmessage=({data})=>{
        if(data?.kind==="configure")this.mixer.configure(data.epoch,data.enabled);
        else if(data?.kind==="playback") {
          this.mixer.enqueue(data);
          // The controller permits only a fixed number of unacknowledged
          // transfers, bounding this MessagePort even if processing suspends.
          this.port.postMessage({kind:"queued",epoch:data.epoch,job:data.job});
        }
        else if(data?.kind==="remove")this.mixer.remove(data.speaker);
      };
    }
    process(_inputs,outputs) {
      const bus=outputs[0];
      if(bus?.[0])this.mixer.render(bus[0],bus[1]||null);
      for(let channel=2;channel<(bus?.length||0);channel++)bus[channel].fill(0);
      return true;
    }
  }
  registerProcessor("nuschl-capture",CaptureProcessor);
  registerProcessor("nuschl-playback",PlaybackProcessor);
}
