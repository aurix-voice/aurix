import vm from 'node:vm';
import { resolveObjectURL } from 'node:buffer';

/** Web Audio stand-in recording the graph topology and the last scheduled parameter values. */
export class FakeAudioContext {
  static all = [];
  constructor() {
    this.currentTime = 0;
    this.state = 'suspended';
    this.sampleRate = 48000;
    this.created = [];
    this.destination = this.node('destination');
    this.closed = false;
    this.sinkId = undefined;
    /** processor name → class registered by a worklet module run through {@link audioWorklet}. */
    this.processors = new Map();
    this.modules = [];
    const ctx = this;
    this.audioWorklet = {
      async addModule(url) {
        const blob = resolveObjectURL(url);
        if (!blob) throw new Error(`no blob behind ${url}`);
        const source = await blob.text();
        ctx.modules.push(source);
        const scope = {
          sampleRate: ctx.sampleRate,
          currentTime: 0,
          AudioWorkletProcessor: FakeAudioWorkletProcessor,
          registerProcessor(name, cls) {
            ctx.processors.set(name, cls);
          },
          Float32Array,
          Math,
          Array,
          Object,
          Error,
          Number,
          Uint8Array,
          console,
        };
        scope.globalThis = scope;
        vm.runInNewContext(source, scope, { filename: url });
      },
    };
    FakeAudioContext.all.push(this);
  }
  node(kind) {
    const ctx = this;
    const n = {
      kind,
      outputs: new Set(),
      connect(dest) {
        this.outputs.add(dest);
        return dest;
      },
      disconnect(dest) {
        if (dest === undefined) this.outputs.clear();
        else this.outputs.delete(dest);
      },
    };
    ctx.created.push(n);
    return n;
  }
  param(value) {
    return {
      value,
      setTargetAtTime(target) {
        this.value = target;
      },
      setValueAtTime(target) {
        this.value = target;
      },
      cancelScheduledValues() {},
    };
  }
  createGain() {
    return Object.assign(this.node('gain'), { gain: this.param(1) });
  }
  createPanner() {
    return Object.assign(this.node('panner'), {
      panningModel: 'equalpower',
      distanceModel: 'inverse',
      refDistance: 1,
      maxDistance: 10000,
      rolloffFactor: 1,
      positionX: this.param(0),
      positionY: this.param(0),
      positionZ: this.param(0),
    });
  }
  createMediaStreamSource(stream) {
    return Object.assign(this.node('source'), { stream });
  }
  createMediaStreamDestination() {
    const track = { kind: 'audio', enabled: true, readyState: 'live', stop() { this.readyState = 'ended'; } };
    const stream = { fake: 'destination', getAudioTracks: () => [track], getTracks: () => [track] };
    return Object.assign(this.node('mediaStreamDestination'), { stream });
  }
  async resume() {
    this.state = 'running';
  }
  async close() {
    this.closed = true;
    this.state = 'closed';
  }
  async setSinkId(id) {
    this.sinkId = id;
  }
}

/** One end of a `MessagePort` pair; messages are structured-cloned and delivered on a microtask. */
class FakePort {
  constructor() {
    this.onmessage = null;
    this.peer = undefined;
    this.closed = false;
  }
  postMessage(data) {
    const peer = this.peer;
    if (!peer || this.closed) return;
    const copy = structuredClone(data);
    queueMicrotask(() => {
      if (!peer.closed) peer.onmessage?.({ data: copy });
    });
  }
  close() {
    this.closed = true;
  }
}

export class FakeAudioWorkletProcessor {
  constructor() {
    this.port = new FakePort();
  }
}

/**
 * `AudioWorkletNode` stand-in: instantiates the processor the module registered under `name`
 * and lets a test push audio through it with {@link FakeAudioWorkletNode.render}.
 */
export class FakeAudioWorkletNode {
  constructor(ctx, name, options = {}) {
    const Processor = ctx.processors.get(name);
    if (!Processor) throw new Error(`processor "${name}" is not registered on this context`);
    this.kind = `worklet:${name}`;
    this.outputs = new Set();
    this.context = ctx;
    this.options = options;
    this.processor = new Processor();
    this.port = new FakePort();
    this.port.peer = this.processor.port;
    this.processor.port.peer = this.port;
    ctx.created.push(this);
  }
  connect(dest) {
    this.outputs.add(dest);
    return dest;
  }
  disconnect(dest) {
    if (dest === undefined) this.outputs.clear();
    else this.outputs.delete(dest);
  }
  /** Run one render quantum: `inputs` is `Float32Array[]` per channel; returns the output channels. */
  render(inputs, outputChannels = inputs.length, frames = inputs[0]?.length ?? 128) {
    const outputs = Array.from({ length: outputChannels }, () => new Float32Array(frames));
    this.processor.process([inputs], this.options.numberOfOutputs === 0 ? [] : [outputs], {});
    return outputs;
  }
}

/** Wait until every message posted so far has been delivered. */
export const flushPorts = () => new Promise((r) => setTimeout(r, 0));
