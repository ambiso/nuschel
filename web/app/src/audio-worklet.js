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
  constructor() {
    this.epoch = ""; this.enabled = false; this.speakers = new Map();
    // Persistent wide sums: render never allocates PCM scratch or accumulates
    // into f32 output until every contributor has been summed.
    this.left = new Float64Array(MAX_QUEUED_SAMPLES);
    this.right = new Float64Array(MAX_QUEUED_SAMPLES);
  }
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
      state = {data:new Float32Array(MAX_QUEUED_SAMPLES * 2), gains:new Float32Array(MAX_QUEUED_SAMPLES), read:0, size:0, idle:0, hold:AUDIO_FRAME};
      this.speakers.set(speaker,state);
    }
    const overflow = Math.max(0,state.size + frames - MAX_QUEUED_SAMPLES);
    state.read = (state.read + overflow) % MAX_QUEUED_SAMPLES;
    state.size -= overflow;
    for (let i=0;i<frames;i++) {
      const at=((state.read + state.size + i) % MAX_QUEUED_SAMPLES) * 2;
      state.gains[at/2] = volume;
      for(let channel=0;channel<2;channel++) {
        // Store unscaled finite PCM. Multiplication happens as f64 during mix;
        // even the largest finite f32 sample ×4 cannot overflow that sum.
        state.data[at+channel] = samples[i*channels+(channels===2?channel:0)];
      }
    }
    state.size += frames; state.idle = 0;
    return true;
  }
  remove(speaker) { this.speakers.delete(speaker); }
  render(left,right=null) {
    if (!this.enabled || left.length > MAX_QUEUED_SAMPLES || (right && right.length !== left.length)) {
      left.fill(0);right?.fill(0);return;
    }
    this.left.fill(0,0,left.length);this.right.fill(0,0,left.length);
    for (const [speaker,state] of this.speakers) {
      const offset=Math.min(left.length,state.hold);state.hold-=offset;
      const length = Math.min(left.length-offset,state.size);
      for(let i=0;i<length;i++) {
        const at=((state.read+i)%MAX_QUEUED_SAMPLES)*2;
        const gain=state.gains[at/2];
        this.left[offset+i]+=state.data[at]*gain;
        this.right[offset+i]+=state.data[at+1]*gain;
      }
      state.read=(state.read+length)%MAX_QUEUED_SAMPLES;state.size-=length;
      state.idle=state.size===0?state.idle+left.length:0;
      if(state.idle>AUDIO_RATE) this.speakers.delete(speaker);
    }
    for(let i=0;i<left.length;i++) {
      left[i]=Math.max(-1,Math.min(1,right?this.left[i]:(this.left[i]+this.right[i])*.5));
      if(right)right[i]=Math.max(-1,Math.min(1,this.right[i]));
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

export const PLAYOUT_FRAME = 480;
export const PLAYOUT_RESERVE = PLAYOUT_FRAME * 2;

// Audio-frame demand owns this lane. In particular, a missed deadline does
// not grant another credit: only an exactly bound reply acknowledges demand.
export class PlayoutClock {
  constructor(report, rate) {
    this.report=report;this.rate=rate;this.port=null;this.configured=false;
    this.epoch="";this.enabled=false;this.generation=0;this.request=0;
    this.cursor=0;this.nextStart=0;this.inFlight=null;this.ready=false;
    this.failed=false;this.errorSent=false;
    this.left=new Float64Array(PLAYOUT_RESERVE);
    this.right=new Float64Array(PLAYOUT_RESERVE);
  }
  clear() { this.left.fill(0);this.right.fill(0); }
  fail(message) {
    this.failed=true;this.ready=false;this.enabled=false;this.inFlight=null;this.clear();
    if(!this.errorSent) {
      this.errorSent=true;this.report({kind:"playout-error",epoch:this.epoch,message});
    }
  }
  configure(epoch,enabled) {
    if(typeof epoch!=="string"||epoch.length>200||typeof enabled!=="boolean"||(!epoch&&enabled)) {
      this.fail("Invalid playback configuration.");return;
    }
    if(this.configured&&epoch===this.epoch&&enabled===this.enabled)return;
    this.configured=true;this.epoch=epoch;this.enabled=enabled;
    this.cursor=0;this.failed=false;this.errorSent=false;
    if(this.rate!==AUDIO_RATE) { this.fail("Playback requires a 48 kHz audio context.");return; }
    this.handshake();
  }
  attach(port) {
    if(!port||typeof port.postMessage!=="function") {this.fail("Invalid playback port.");return;}
    if(this.port===port)return;
    if(this.port) {this.port.onmessage=null;this.port.close?.();}
    this.port=port;port.onmessage=({data})=>this.message(data);port.start?.();
    if(this.configured&&!this.failed)this.handshake();
  }
  close() {
    this.enabled=false;this.ready=false;this.inFlight=null;this.clear();
    if(this.port) {this.port.onmessage=null;this.port.close?.();this.port=null;}
  }
  handshake() {
    this.clear();this.ready=false;this.inFlight=null;
    if(this.generation>=Number.MAX_SAFE_INTEGER) {this.fail("Playback clock exhausted.");return;}
    this.generation++;
    this.nextStart=Math.ceil(this.cursor/PLAYOUT_FRAME)*PLAYOUT_FRAME+PLAYOUT_FRAME;
    this.port?.postMessage({kind:"playout-configure",epoch:this.epoch,enabled:this.enabled,clock_generation:this.generation});
  }
  message(data) {
    if(this.failed||!data||data.epoch!==this.epoch||data.clock_generation!==this.generation)return;
    if(data.kind==="playout-ready") {
      if(typeof data.enabled!=="boolean")return;
      const ready=Boolean(this.enabled&&data.enabled);
      if(ready&&!this.ready&&!this.inFlight)
        this.nextStart=Math.ceil(this.cursor/PLAYOUT_FRAME)*PLAYOUT_FRAME+PLAYOUT_FRAME;
      this.ready=ready;
      if(!this.ready)this.clear();
      return;
    }
    const pending=this.inFlight;
    if(data.kind!=="playout"||!pending||data.request!==pending.request||
        data.start_frame!==pending.start_frame)return;
    // Validate the entire batch before changing credit or accumulation state.
    if(data.frames!==PLAYOUT_FRAME||!Array.isArray(data.speakers)||data.speakers.length>MAX_SPEAKERS) {
      this.fail("Invalid playback batch.");return;
    }
    const ids=new Set();
    for(const speaker of data.speakers) {
      if(!speaker||!Number.isInteger(speaker.speaker)||speaker.speaker<0||speaker.speaker>0xffffffff||
          ids.has(speaker.speaker)||speaker.channels!==2||speaker.frames!==PLAYOUT_FRAME||
          !(speaker.samples instanceof Float32Array)||speaker.samples.length!==PLAYOUT_FRAME*2||
          !Number.isFinite(speaker.volume)||speaker.volume<0||speaker.volume>4||
          speaker.samples.some(value=>!Number.isFinite(value))) {
        this.fail("Invalid playback speaker.");return;
      }
      ids.add(speaker.speaker);
    }
    this.inFlight=null;
    if(data.start_frame+PLAYOUT_FRAME<=this.cursor) {this.handshake();return;}
    if(!this.enabled||!this.ready)return;
    const elapsed=Math.max(0,this.cursor-data.start_frame);
    for(const speaker of data.speakers)for(let i=elapsed;i<PLAYOUT_FRAME;i++) {
      const at=(data.start_frame+i)%PLAYOUT_RESERVE;
      this.left[at]+=speaker.samples[i*2]*speaker.volume;
      this.right[at]+=speaker.samples[i*2+1]*speaker.volume;
    }
  }
  render(left,right=null) {
    left.fill(0);right?.fill(0);
    if(this.failed||!this.enabled)return;
    if(!Number.isSafeInteger(this.cursor+left.length+PLAYOUT_RESERVE)) {
      this.fail("Playback clock exhausted.");return;
    }
    if(this.ready&&this.port&&!this.inFlight&&this.nextStart<=this.cursor+PLAYOUT_FRAME) {
      if(this.request>=Number.MAX_SAFE_INTEGER) {this.fail("Playback request exhausted.");return;}
      const pending={kind:"render",epoch:this.epoch,clock_generation:this.generation,
        request:++this.request,start_frame:this.nextStart,frames:PLAYOUT_FRAME};
      this.inFlight={...pending,sentAt:this.cursor,stalled:false};
      this.nextStart+=PLAYOUT_FRAME;
      this.port.postMessage(pending);
    }
    for(let i=0;i<left.length;i++) {
      const at=(this.cursor+i)%PLAYOUT_RESERVE;
      left[i]=Math.max(-1,Math.min(1,right?this.left[at]:(this.left[at]+this.right[at])*.5));
      if(right)right[i]=Math.max(-1,Math.min(1,this.right[at]));
      this.left[at]=0;this.right[at]=0;
    }
    this.cursor+=left.length;
    const pending=this.inFlight;
    if(pending&&!pending.stalled&&this.cursor-pending.sentAt>=AUDIO_RATE*2.5) {
      pending.stalled=true;
      this.report({kind:"playout-stalled",epoch:this.epoch,clock_generation:this.generation,
        request:pending.request,start_frame:pending.start_frame});
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
      super();this.mixer=new PcmMixer();this.clocked=false;
      this.clock=new PlayoutClock(message=>this.port.postMessage(message),globalThis.sampleRate);
      this.port.onmessage=({data})=>{
        if(data?.kind==="playout-port")this.clock.attach(data.port);
        else if(data?.kind==="close") {this.clock.close();this.mixer.configure("",false);}
        else if(data?.kind==="configure") {
          this.clocked=data.clocked===true;
          if(this.clocked) {this.mixer.configure("",false);this.clock.configure(data.epoch,data.enabled);}
          else {this.clock.close();this.mixer.configure(data.epoch,data.enabled);}
        }
        else if(data?.kind==="playback"&&!this.clocked) {
          this.mixer.enqueue(data);
          // The controller permits only a fixed number of unacknowledged
          // transfers, bounding this MessagePort even if processing suspends.
          this.port.postMessage({kind:"queued",epoch:data.epoch,job:data.job});
        }
        else if(data?.kind==="remove"&&!this.clocked)this.mixer.remove(data.speaker);
      };
    }
    process(_inputs,outputs) {
      const bus=outputs[0];
      if(bus?.[0]) {
        if(this.clocked)this.clock.render(bus[0],bus[1]||null);
        else this.mixer.render(bus[0],bus[1]||null);
      }
      for(let channel=2;channel<(bus?.length||0);channel++)bus[channel].fill(0);
      return true;
    }
  }
  registerProcessor("nuschl-capture",CaptureProcessor);
  registerProcessor("nuschl-playback",PlaybackProcessor);
}
