/** Web Audio stand-in recording the graph topology and the last scheduled parameter values. */
export class FakeAudioContext {
  constructor() {
    this.currentTime = 0;
    this.state = 'suspended';
    this.created = [];
    this.destination = this.node('destination');
    this.closed = false;
    this.sinkId = undefined;
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
      disconnect() {
        this.outputs.clear();
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

