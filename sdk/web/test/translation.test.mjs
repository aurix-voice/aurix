import { test } from 'node:test';
import assert from 'node:assert/strict';
import { AurixClient } from '../dist/client.js';
import { parseServerMessage } from '../dist/protocol.js';

/** Minimal WebSocket stand-in: records what the client sends and lets the test inject server frames. */
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
    this.sent.push(JSON.parse(data));
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

const ack = (extra = {}) => ({
  type: 'SessionInitAck',
  data: { session_id: 's1', ssrc: 7, media_addr: '10.0.0.2:40000', media_key: 'AAAA', ...extra },
});

function newClient() {
  globalThis.WebSocket = FakeSocket;
  const client = new AurixClient({
    apiUrl: 'http://api',
    wsUrl: 'wss://a/ws',
    token: 't',
    autoReconnect: false,
    pingIntervalMs: 0,
  });
  client.on('error', () => {});
  return client;
}

/** Drive `connect()` up to the session ack; media setup fails in Node, which is fine for these tests. */
async function connectWithAck(client, ackExtra) {
  const connecting = client.connect().catch(() => undefined);
  await new Promise((r) => setTimeout(r, 0));
  FakeSocket.last.receive(ack(ackExtra));
  const ready = new Promise((resolve) => client.on('sessionReady', resolve));
  await connecting;
  return ready;
}

test('SessionInitAck exposes the translation capability; older nodes leave it undefined', async () => {
  const client = newClient();
  const info = await connectWithAck(client, {
    translation: { speech: true, languages: ['en', 'de', 'ja'] },
  });
  assert.deepEqual(info.translation, { speech: true, languages: ['en', 'de', 'ja'] });

  const legacy = newClient();
  const legacyInfo = await connectWithAck(legacy, {});
  assert.equal(legacyInfo.translation, undefined);
});

test('setTranslation normalises tags, is held client-side and replayed after the session ack', async () => {
  const client = newClient();
  client.setTranslation(' DE_de ', { spokenLanguage: 'EN', speech: true });
  assert.deepEqual(client.translationPrefs, { language: 'de-de', spokenLanguage: 'en', speech: true });

  await connectWithAck(client, { translation: { speech: true } });
  const replayed = FakeSocket.last.sent.filter((m) => m.type === 'SetTranslation');
  assert.deepEqual(replayed, [
    { type: 'SetTranslation', data: { language: 'de-de', spoken_language: 'en', speech: true } },
  ]);
});

test('speech is only requested together with a target language; clearing sends an empty preference', async () => {
  const client = newClient();
  const connecting = client.connect().catch(() => undefined);
  await new Promise((r) => setTimeout(r, 0));
  const sock = FakeSocket.last;
  sock.receive(ack());

  client.setTranslation(undefined, { speech: true });
  assert.deepEqual(client.translationPrefs, { speech: false });

  client.setTranslation(undefined, { spokenLanguage: 'ru' });
  assert.deepEqual(client.translationPrefs, { spokenLanguage: 'ru', speech: false });
  assert.deepEqual(
    sock.sent.filter((m) => m.type === 'SetTranslation').map((m) => m.data),
    [{ speech: false }, { spoken_language: 'ru', speech: false }],
  );
  await connecting;
});

test('TranslationChanged updates the held preference and is emitted', async () => {
  const client = newClient();
  await connectWithAck(client, {});
  const seen = [];
  client.on('translationChanged', (p) => seen.push(p));
  FakeSocket.last.receive({
    type: 'TranslationChanged',
    data: { language: 'fr', spoken_language: null, speech: false },
  });
  FakeSocket.last.receive({ type: 'TranslationChanged', data: { language: null, speech: false } });
  assert.deepEqual(seen, [{ language: 'fr', speech: false }, { speech: false }]);
  assert.deepEqual(client.translationPrefs, { speech: false });
});

test('translated transcripts carry the original text and language', () => {
  const msg = parseServerMessage(
    JSON.stringify({
      type: 'Transcript',
      data: {
        transcript: {
          id: 't1',
          channel_id: 'c1',
          user_id: 'alice',
          text: 'Hallo Welt',
          language: 'de',
          started_at: '2024-01-01T00:00:00Z',
          duration_ms: 900,
          original: { text: 'hello world', language: 'en' },
        },
      },
    }),
  );
  assert.equal(msg.type, 'Transcript');
  assert.deepEqual(msg.data.transcript.original, { text: 'hello world', language: 'en' });

  const untranslated = parseServerMessage(
    JSON.stringify({
      type: 'Transcript',
      data: {
        transcript: {
          id: 't2',
          channel_id: 'c1',
          user_id: 'alice',
          text: 'hello world',
          started_at: '2024-01-01T00:00:00Z',
          duration_ms: 900,
        },
      },
    }),
  );
  assert.equal(untranslated.data.transcript.original, undefined);
});
