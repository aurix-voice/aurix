import { AurixClient } from '../../dist/client.js';
import { FakeAudioContext } from './fake-audio.mjs';

// ── Browser stand-ins: control socket, WebRTC peer connection, media streams ──

export class FakeSocket {
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

export class FakeTrack extends EventTarget {
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

export class FakeMediaStream {
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

export class FakePeerConnection extends EventTarget {
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

export const ack = (extra = {}) => ({
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

export const brief = (user_id, ssrc) => ({ user_id, display_name: user_id, ssrc, role: 'speaker', is_muted: false, is_speaking: false });

export const facingZ = { forward_x: 0, forward_y: 0, forward_z: 1, up_x: 0, up_y: 1, up_z: 0 };
export const positional = {
  near_distance: 1,
  far_distance: 50,
  rolloff: 'logarithmic',
  max_radius: 100,
  directional: true,
  coordinate_system: 'left_handed',
};

/** Answer every offer the client sends so media negotiation completes. */
export function autoAnswer(sock) {
  sock.onsent = (msg) => {
    if (msg.type === 'WebRtcOffer') queueMicrotask(() => sock.receive({ type: 'WebRtcAnswer', data: { sdp: 'v=0\r\nanswer' } }));
  };
}

export async function connected(opts = {}, ackExtra = {}) {
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

export async function joined(client, sock, channelId, participants, extra = {}) {
  const joining = client.joinChannel(channelId);
  await new Promise((r) => setTimeout(r, 0));
  sock.receive({ type: 'ChannelJoinAck', data: { channel_id: channelId, participants, ...extra } });
  return joining;
}
