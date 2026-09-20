import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AurixClient } from '../dist/client.js';
import { FakeAudioContext } from './helpers/fake-audio.mjs';

// ── Browser stand-ins: control socket, WebRTC peer connection, media streams ──

class FakeSocket {
  static OPEN = 1;
  static CLOSED = 3;
  static last;
  constructor(url, protocols) {
    this.url = url;
    this.protocols = protocols;
    this.readyState = FakeSocket.OPEN;
    this.sent = [];
    FakeSocket.last = this;
    queueMicrotask(() => this.onopen?.({}));
  }
  send(data) {
    const msg = JSON.parse(data);
    this.sent.push(msg);
    this.onsent?.(msg);
  }
  close(code = 1000, reason = '') {
    if (this.readyState === FakeSocket.CLOSED) return;
    this.readyState = FakeSocket.CLOSED;
    this.onclose?.({ code, reason });
  }
  receive(msg) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

class FakeTrack extends EventTarget {
  constructor(kind = 'audio') {
    super();
    this.kind = kind;
    this.enabled = true;
    this.readyState = 'live';
  }
  stop() {
    this.readyState = 'ended';
  }
}

class FakeMediaStream {
  constructor(tracks = []) {
    this.tracks = tracks;
    this.mid = undefined;
  }
  getAudioTracks() {
    return this.tracks.filter((t) => t.kind === 'audio');
  }
  getTracks() {
    return [...this.tracks];
  }
}

class FakePeerConnection extends EventTarget {
  static all = [];
  constructor(config) {
    super();
    this.config = config;
    this.transceivers = [];
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    this.remoteDescription = null;
    this.closed = false;
    FakePeerConnection.all.push(this);
  }
  addTransceiver(trackOrKind, init = {}) {
    const t = {
      mid: null,
      direction: init.direction ?? 'sendrecv',
      sender: {
        track: typeof trackOrKind === 'string' ? null : trackOrKind,
        getParameters: () => ({ encodings: [{}] }),
        setParameters: async () => undefined,
        replaceTrack: async () => undefined,
      },
      receiver: { track: new FakeTrack() },
    };
    this.transceivers.push(t);
    return t;
  }
  getTransceivers() {
    return [...this.transceivers];
  }
  getSenders() {
    return this.transceivers.map((t) => t.sender);
  }
  async createOffer() {
    this.transceivers.forEach((t, i) => {
      t.mid = String(i);
    });
    const lines = this.transceivers.map((t) => `m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:${t.mid}\r\na=${t.direction}\r\n`);
    return { type: 'offer', sdp: `v=0\r\n${lines.join('')}` };
  }
  async setLocalDescription(desc) {
    this.localDescription = desc;
  }
  async setRemoteDescription(desc) {
    this.remoteDescription = desc;
  }
  /** The server sends on transceiver `mid`: the browser fires `track`. */
  arrive(mid) {
    const t = this.transceivers.find((x) => x.mid === mid);
    const stream = new FakeMediaStream([t.receiver.track]);
    stream.mid = mid;
    this.ontrack?.({ track: t.receiver.track, streams: [stream], transceiver: t });
    return stream;
  }
  connect() {
    this.connectionState = 'connected';
    this.onconnectionstatechange?.();
  }
  close() {
    this.closed = true;
    this.connectionState = 'closed';
  }
}

globalThis.WebSocket = FakeSocket;
globalThis.RTCPeerConnection = FakePeerConnection;
globalThis.MediaStream = FakeMediaStream;

const ack = (extra = {}) => ({
  type: 'SessionInitAck',
  data: {
    session_id: 's1',
    ssrc: 7,
    media_addr: '10.0.0.2:40000',
    media_key: 'AAAA',
    webrtc_participant_streams: 2,
    unfocused_channel_gain: 0.4,
    ...extra,
  },
});

const brief = (user_id, ssrc) => ({ user_id, display_name: user_id, ssrc, role: 'speaker', is_muted: false, is_speaking: false });

const facingZ = { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 };
const positional = {
  near_distance: 1,
  far_distance: 50,
  rolloff: 'logarithmic',
  max_radius: 100,
  directional: true,
  coordinate_system: 'left_handed',
};

/** Answer every offer the client sends so media negotiation completes. */
function autoAnswer(sock) {
  sock.onsent = (msg) => {
    if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
  };
}

async function connected(opts = {}, ackExtra = {}) {
  const ctx = new FakeAudioContext();
  const client = new AurixClient({
    apiUrl: 'http://api',
    wsUrl: 'wss://a/ws',
    token: 't',
    autoReconnect: false,
    pingIntervalMs: 0,
    qualityReportIntervalMs: 0,
    useTurn: false,
    localVoiceActivity: false,
    localStream: new FakeMediaStream([new FakeTrack()]),
    audioContext: ctx,
    ...opts,
  });
  client.on('error', () => {});
  const connecting = client.connect();
  await new Promise((r) => setTimeout(r, 0));
  const sock = FakeSocket.last;
  autoAnswer(sock);
  sock.receive(ack(ackExtra));
  await connecting;
  const pc = FakePeerConnection.all.at(-1);
  pc.connect();
  return { client, sock, pc, ctx };
}

async function joined(client, sock, channelId, participants, extra = {}) {
  const joining = client.joinChannel(channelId);
  await new Promise((r) => setTimeout(r, 0));
  sock.receive({ type: 'ChannelJoinAck', data: { channel_id: channelId, participants, ...extra } });
  return joining;
}

const gainOf = (ctx, mid) => {
  const source = ctx.created.find((n) => n.kind === 'source' && n.stream.mid === mid);
  return [...source.outputs][0].gain.value;
};

test('offers the mixed track plus recvonly tracks up to the node cap and maps ParticipantStreams onto them', async () => {
  const { client, sock, pc, ctx } = await connected({ participantStreams: 5 });
  assert.equal(client.participantStreamCap, 2);
  assert.deepEqual(
    pc.transceivers.map((t) => t.direction),
    ['sendrecv', 'recvonly', 'recvonly'],
    'wish of 5 capped to the node’s 2',
  );
  assert.equal(client.negotiatedParticipantStreams, 0, 'tracks count once they arrive');

  const layouts = [];
  client.on('participantStreams', (s) => layouts.push(s));
  const remote = [];
  client.on('remoteStream', (s) => remote.push(s));
  pc.arrive('0');
  assert.equal(remote.length, 1, 'mid 0 is the mixed downlink');
  pc.arrive('1');
  pc.arrive('2');
  assert.equal(client.negotiatedParticipantStreams, 2);
  assert.equal(remote.length, 1);
  assert.deepEqual(
    layouts.at(-1).map((s) => [s.mid, s.userId]),
    [
      ['1', undefined],
      ['2', undefined],
    ],
  );
  // Idle tracks are silent in the graph even if the server were to send on them.
  assert.equal(gainOf(ctx, '1'), 0);

  await joined(client, sock, 'c1', [brief('me', 7), brief('alice', 8), brief('bob', 9)]);
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'alice' }, { mid: '2', user_id: null }] } });
  assert.deepEqual(
    client.getParticipantStreams().map((s) => [s.mid, s.userId, s.stream?.mid]),
    [
      ['1', 'alice', '1'],
      ['2', undefined, '2'],
    ],
  );
  assert.equal(client.getParticipantStream('alice').mid, '1');
  assert.equal(client.getParticipantStream('bob'), undefined);
  assert.equal(gainOf(ctx, '1'), 1, 'alice is heard as sent');
  assert.equal(gainOf(ctx, '2'), 0);

  // Receiver-local controls are mirrored onto the dedicated track (the server drops the
  // frames too, this keeps a track that changes hands from leaking the previous gain).
  client.setParticipantVolume('alice', 0.5);
  assert.equal(gainOf(ctx, '1'), 0.5);
  client.setParticipantMuted('alice', true, 'c1');
  assert.equal(gainOf(ctx, '1'), 0);
  client.setParticipantMuted('alice', false, 'c1');
  assert.equal(gainOf(ctx, '1'), 0.5);
  sock.receive({ type: 'UserBlockChanged', data: { user_id: 'alice', blocked: true } });
  assert.equal(gainOf(ctx, '1'), 0);
  sock.receive({ type: 'UserBlockChanged', data: { user_id: 'alice', blocked: false } });
  assert.equal(gainOf(ctx, '1'), 0.5);

  // Focus on another channel dims alice by the node's unfocused gain.
  await joined(client, sock, 'c2', [brief('me', 7)]);
  client.setChannelFocus('c2');
  assert.ok(Math.abs(gainOf(ctx, '1') - 0.5 * 0.4) < 1e-9);
  client.setChannelFocus(undefined);
  assert.equal(gainOf(ctx, '1'), 0.5);

  // The track changes hands: bob's parameters, not alice's.
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'bob' }, { mid: '2', user_id: 'alice' }] } });
  assert.equal(gainOf(ctx, '1'), 1);
  assert.equal(gainOf(ctx, '2'), 0.5);
  assert.equal(client.isParticipantSpatialized('bob'), false, 'team channel: no panner');

  // A track the browser ends drops out of the layout snapshot.
  pc.transceivers[2].receiver.track.dispatchEvent(new Event('ended'));
  assert.equal(client.negotiatedParticipantStreams, 1);
  assert.equal(layouts.at(-1).find((s) => s.mid === '2').stream, undefined);
  client.disconnect();
});

test('positional channels render dedicated tracks through the HRTF panner with the server’s attenuation', async () => {
  const { client, sock, pc, ctx } = await connected();
  pc.arrive('0');
  pc.arrive('1');
  pc.arrive('2');
  await joined(client, sock, 'zone', [brief('me', 7), brief('alice', 8), brief('bob', 9)], { positional });
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'alice' }, { mid: '2', user_id: 'bob' }] } });
  // No positions yet: no attenuation, no panner (like the server, which forwards nothing
  // for position-less members of a positional channel — so this path never carries audio).
  assert.equal(client.isParticipantSpatialized('alice'), false);

  client.updatePosition('zone', { x: 0, y: 0, z: 0 }, facingZ);
  sock.receive({
    type: 'PositionUpdate',
    data: {
      channel_id: 'zone',
      positions: [
        { user_id: 'alice', position: { x: 4, y: 0, z: 0 }, orientation: facingZ },
        { user_id: 'bob', position: { x: 0, y: 0, z: 200 }, orientation: facingZ },
      ],
    },
  });
  assert.equal(client.isParticipantSpatialized('alice'), true);
  assert.ok(Math.abs(gainOf(ctx, '1') - 0.25) < 1e-9, 'logarithmic rolloff at 4 m');
  const panner = ctx.created.find((n) => n.kind === 'panner');
  assert.equal(panner.panningModel, 'HRTF');
  assert.ok(Math.abs(panner.positionX.value - 1) < 1e-9, 'alice is to the right');
  assert.equal(client.isParticipantSpatialized('bob'), false, 'out of range: inaudible');
  assert.equal(gainOf(ctx, '2'), 0);

  // The listener turns around: alice is now on the left.
  client.updatePosition('zone', { x: 0, y: 0, z: 0 }, { ...facingZ, forward_z: -1 });
  assert.ok(Math.abs(panner.positionX.value + 1) < 1e-9);

  // Leaving the positional channel forgets positions; alice's track (still assigned by the
  // server until it says otherwise) falls back to plain gain.
  client.leaveChannel('zone');
  assert.equal(client.isParticipantSpatialized('alice'), false);
  client.disconnect();
});

test('pinned participants are validated against the cap, sent once media is up and replayed after renegotiation', async () => {
  const { client, sock, pc } = await connected({ autoReconnect: true });
  assert.throws(() => client.setPinnedParticipants(['a', 'b', 'c']), RangeError);
  client.setPinnedParticipants(['alice', 'alice', 'bob']);
  assert.deepEqual(client.getPinnedParticipants(), ['alice', 'bob']);
  assert.deepEqual(
    sock.sent.filter((m) => m.type === 'SetParticipantStreams').map((m) => m.data),
    [{ pinned: ['alice', 'bob'] }],
  );

  pc.arrive('0');
  pc.arrive('1');
  pc.arrive('2');
  sock.receive({ type: 'ParticipantStreams', data: { streams: [{ mid: '1', user_id: 'alice' }, { mid: '2', user_id: 'bob' }] } });
  assert.equal(client.negotiatedParticipantStreams, 2);

  // ICE failure with a live control channel: the client renegotiates media itself. The old
  // tracks and layout go, the new offer carries the recvonly tracks and the pins again.
  const layouts = [];
  client.on('participantStreams', (s) => layouts.push(s));
  const before = sock.sent.length;
  pc.connectionState = 'failed';
  pc.onconnectionstatechange();
  await new Promise((r) => setTimeout(r, 5));
  assert.equal(pc.closed, true);
  const pc2 = FakePeerConnection.all.at(-1);
  assert.notEqual(pc2, pc);
  assert.deepEqual(
    pc2.transceivers.map((t) => t.direction),
    ['sendrecv', 'recvonly', 'recvonly'],
  );
  assert.equal(layouts[0].length, 0, 'layout cleared while the new transport comes up');
  assert.equal(client.negotiatedParticipantStreams, 0);
  const replayed = sock.sent.slice(before).filter((m) => m.type === 'SetParticipantStreams');
  assert.deepEqual(replayed.map((m) => m.data), [{ pinned: ['alice', 'bob'] }]);
  // Tracks of the dead connection are ignored even if the browser still reports them.
  pc.arrive('1');
  assert.equal(client.negotiatedParticipantStreams, 0);
  pc2.arrive('1');
  assert.equal(client.negotiatedParticipantStreams, 1);
  client.disconnect();
});

test('participantStreams: 0 or a node without the feature keeps the single mixed track', async () => {
  const off = await connected({ participantStreams: 0 });
  assert.deepEqual(off.pc.transceivers.map((t) => t.direction), ['sendrecv']);
  off.client.disconnect();

  const legacy = await connected({}, { webrtc_participant_streams: undefined });
  assert.equal(legacy.client.participantStreamCap, 0);
  assert.deepEqual(legacy.pc.transceivers.map((t) => t.direction), ['sendrecv']);
  assert.throws(() => legacy.client.setPinnedParticipants(['x']), RangeError);
  legacy.client.disconnect();
});

test('spatialAudio: false negotiates the tracks but leaves rendering to the app', async () => {
  const { client, pc, ctx } = await connected({ spatialAudio: false });
  assert.deepEqual(pc.transceivers.map((t) => t.direction), ['sendrecv', 'recvonly', 'recvonly']);
  const stream = pc.arrive('1');
  assert.equal(client.getParticipantStreams()[0].stream, stream);
  assert.deepEqual(ctx.created.filter((n) => n.kind !== 'destination'), [], 'no Web Audio graph built');
  client.disconnect();
});

test('output volume, mute and device apply to the participant graph too; disconnect closes it', async () => {
  globalThis.HTMLMediaElement = { prototype: { setSinkId() {} } };
  const { client, pc, ctx } = await connected();
  pc.arrive('1');
  const master = ctx.created.find((n) => n.kind === 'gain');
  client.setOutputVolume(0.25);
  assert.ok(Math.abs(master.gain.value - 0.25) < 1e-9);
  client.setOutputMuted(true);
  assert.equal(master.gain.value, 0);
  await client.setOutputDevice('spk');
  assert.equal(ctx.sinkId, 'spk');
  assert.equal(await client.resumeAudio(), true);
  client.disconnect();
  assert.equal(client.negotiatedParticipantStreams, 0);
  assert.equal(ctx.closed, false, 'an app-supplied AudioContext is left open');
});
