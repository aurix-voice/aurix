// The Unity WebGL plugin (sdk/unity/Runtime/Plugins/WebGL/AurixWebGL.jslib) is plain Emscripten
// library JavaScript. These tests evaluate it the way Emscripten does — `mergeInto` + `$Aurix`
// hoisted to a module-scope `Aurix` — on top of a tiny wasm-heap emulation and the real browser
// bundle (dist/aurix-web-sdk.js), so the C# ⇄ JS contract is exercised end to end without Unity.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const jslibUrl = new URL('../../unity/Runtime/Plugins/WebGL/AurixWebGL.jslib', import.meta.url);
const bundleUrl = new URL('../dist/aurix-web-sdk.js', import.meta.url);

/** Emscripten runtime stand-in: UTF-8 heap helpers, a bump allocator and the library registry. */
function emscripten() {
  const heap = new Uint8Array(1 << 20);
  let brk = 8;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const env = {
    HEAPU8: heap,
    LibraryManager: { library: {} },
    deps: [],
    _malloc(size) {
      const ptr = brk;
      brk += size;
      return ptr;
    },
    lengthBytesUTF8: (s) => encoder.encode(s).length,
    stringToUTF8(s, ptr, max) {
      const bytes = encoder.encode(s).subarray(0, Math.max(0, max - 1));
      heap.set(bytes, ptr);
      heap[ptr + bytes.length] = 0;
    },
    UTF8ToString(ptr) {
      if (!ptr) return '';
      let end = ptr;
      while (heap[end] !== 0) end++;
      return decoder.decode(heap.subarray(ptr, end));
    },
    autoAddDeps(lib, dep) {
      env.deps.push(dep);
    },
    mergeInto(target, lib) {
      Object.assign(target, lib);
    }
  };
  // C# marshals `string` arguments as pointers into the heap and reads returned pointers back.
  env.str = (s) => {
    const ptr = env._malloc(env.lengthBytesUTF8(s) + 1);
    env.stringToUTF8(s, ptr, env.lengthBytesUTF8(s) + 1);
    return ptr;
  };
  return env;
}

/**
 * Evaluate the plugin in a fresh context. `page` is what the plugin sees as `window` / `document`
 * (omit `document` to emulate a non-browser host).
 */
function loadPlugin({ withBundle = false, page = {} } = {}) {
  const env = emscripten();
  const ctx = vm.createContext({ ...env, ...page });
  if (page.window === undefined) ctx.window = ctx;
  if (withBundle) vm.runInContext(readFileSync(bundleUrl, 'utf8'), ctx);
  vm.runInContext(readFileSync(jslibUrl, 'utf8'), ctx);
  const lib = ctx.LibraryManager.library;
  assert.deepEqual(ctx.deps, ['$Aurix']);
  // Emscripten turns `$Aurix` into a module-scope variable named `Aurix` that the functions close over.
  ctx.Aurix = lib.$Aurix;
  const api = {};
  for (const [name, fn] of Object.entries(lib)) {
    if (name.startsWith('AurixWebGL_')) api[name] = vm.runInContext(`(${fn.toString()})`, ctx);
  }
  const call = (name, ...args) => api[name](...args.map((a) => (typeof a === 'string' ? env.str(a) : a)));
  const callString = (name, ...args) => env.UTF8ToString(call(name, ...args));
  return { ctx, env, lib, api, call, callString, aurix: lib.$Aurix };
}

const options = JSON.stringify({ apiUrl: 'http://api', wsUrl: 'ws://ws', token: 't' });

test('the plugin registers every entry point Aurix.WebGL.NativeWebGLBridge imports', () => {
  const { lib } = loadPlugin();
  const expected = [
    'AurixWebGL_LoadSdk',
    'AurixWebGL_SdkStatus',
    'AurixWebGL_SdkError',
    'AurixWebGL_Create',
    'AurixWebGL_Invoke',
    'AurixWebGL_Drain',
    'AurixWebGL_Destroy'
  ];
  for (const name of expected) assert.equal(typeof lib[name], 'function', name);
  const source = readFileSync(new URL('../../unity/Runtime/WebGL/WebGLBridge.cs', import.meta.url), 'utf8');
  const imported = [...source.matchAll(/extern \w+ (AurixWebGL_\w+)\(/g)].map((m) => m[1]).sort();
  assert.deepEqual(imported, [...expected].sort());
});

test('without the bundle the plugin reports NotLoaded and fails calls gracefully', () => {
  const { call, callString } = loadPlugin();
  assert.equal(call('AurixWebGL_SdkStatus'), 0);
  assert.equal(callString('AurixWebGL_SdkError'), '');
  assert.equal(call('AurixWebGL_Create', options), 0);
  assert.deepEqual(JSON.parse(callString('AurixWebGL_Invoke', 1, 'connect', '{}', 1)), {
    ok: false,
    error: { message: 'Aurix Web SDK is not loaded', name: 'Error' }
  });
  assert.equal(callString('AurixWebGL_Drain', 1), '[]');
  call('AurixWebGL_Destroy', 1);
});

test('a bundle already on the page is picked up without LoadSdk', () => {
  const { call } = loadPlugin({ withBundle: true });
  assert.equal(call('AurixWebGL_SdkStatus'), 2);
});

test('LoadSdk injects a script tag once and tracks onload / onerror', () => {
  const tags = [];
  const document = {
    createElement: (kind) => {
      const tag = { kind, parentNode: null };
      return tag;
    },
    head: {
      appendChild(tag) {
        tag.parentNode = document.head;
        tags.push(tag);
      },
      removeChild(tag) {
        tags.splice(tags.indexOf(tag), 1);
        tag.parentNode = null;
      }
    }
  };
  const { call, callString, ctx } = loadPlugin({ page: { document } });
  call('AurixWebGL_LoadSdk', 'StreamingAssets/aurix-web-sdk.js');
  assert.equal(call('AurixWebGL_SdkStatus'), 1);
  call('AurixWebGL_LoadSdk', 'StreamingAssets/aurix-web-sdk.js');
  assert.equal(tags.length, 1, 'loading is idempotent');
  assert.equal(tags[0].src, 'StreamingAssets/aurix-web-sdk.js');
  assert.equal(tags[0].async, true);

  // The script ran but did not define the SDK.
  tags[0].onload();
  assert.equal(call('AurixWebGL_SdkStatus'), 3);
  assert.match(callString('AurixWebGL_SdkError'), /AurixBridge is missing/);

  // A retry after failure injects a new tag; a network error is reported and the tag removed.
  call('AurixWebGL_LoadSdk', 'other.js');
  assert.equal(tags.length, 2);
  tags[1].onerror();
  assert.equal(call('AurixWebGL_SdkStatus'), 3);
  assert.equal(callString('AurixWebGL_SdkError'), 'failed to load other.js');
  assert.equal(tags.length, 1);

  // Success: the script defines window.AurixWebSdk before onload fires.
  call('AurixWebGL_LoadSdk', 'aurix-web-sdk.js');
  vm.runInContext(readFileSync(bundleUrl, 'utf8'), ctx);
  tags[tags.length - 1].onload();
  assert.equal(call('AurixWebGL_SdkStatus'), 2);
  assert.equal(callString('AurixWebGL_SdkError'), '');
  assert.ok(call('AurixWebGL_Create', options) > 0);
});

test('LoadSdk outside a browser page fails instead of hanging in Loading', () => {
  const { call, callString } = loadPlugin({ page: { document: undefined } });
  call('AurixWebGL_LoadSdk', 'x.js');
  assert.equal(call('AurixWebGL_SdkStatus'), 3);
  assert.match(callString('AurixWebGL_SdkError'), /no document/);
});

test('Create / Invoke / Drain / Destroy round-trip JSON through the real bundle', async () => {
  const { call, callString } = loadPlugin({ withBundle: true });
  const handle = call('AurixWebGL_Create', options);
  assert.ok(handle > 0);
  assert.deepEqual(JSON.parse(callString('AurixWebGL_Invoke', handle, 'connectionState', '{}', 0)), {
    ok: true,
    value: 'disconnected'
  });
  assert.deepEqual(JSON.parse(callString('AurixWebGL_Invoke', handle, 'isMuted', '{}', 0)), { ok: true, value: false });
  // Non-ASCII survives both UTF-8 crossings.
  const name = 'Привет, мир — 🎧';
  const echo = JSON.parse(callString('AurixWebGL_Invoke', handle, 'provideToken', JSON.stringify({ requestId: 1, token: name }), 0));
  assert.equal(echo.ok, false, 'unknown token request is rejected, but the string was parsed');
  assert.equal(echo.error.message, 'unknown token request 1');

  // An async call: pending now, a `result` event later (connect fails: no WebSocket in Node).
  assert.deepEqual(JSON.parse(callString('AurixWebGL_Invoke', handle, 'connect', '{}', 7)), { ok: true, pending: true });
  assert.deepEqual(JSON.parse(callString('AurixWebGL_Drain', handle)), [{ type: 'connectionState', state: 'connecting' }]);
  await new Promise((r) => setTimeout(r, 20));
  const events = JSON.parse(callString('AurixWebGL_Drain', handle));
  assert.deepEqual(
    events.filter((e) => e.type === 'connectionState'),
    [{ type: 'connectionState', state: 'failed' }],
    'a failed connect settles the state instead of leaving it at "connecting"'
  );
  const result = events.find((e) => e.type === 'result');
  assert.equal(result.rid, 7);
  assert.equal(result.ok, false);
  assert.equal(typeof result.error.message, 'string');
  assert.equal(callString('AurixWebGL_Drain', handle), '[]');

  call('AurixWebGL_Destroy', handle);
  const gone = JSON.parse(callString('AurixWebGL_Invoke', handle, 'connectionState', '{}', 0));
  assert.equal(gone.ok, false);
  assert.match(gone.error.message, /unknown handle/);
  assert.equal(callString('AurixWebGL_Drain', handle), '[]', 'drain of a destroyed handle is empty, not an exception');
  call('AurixWebGL_Destroy', handle); // double free is a no-op
});

test('bad options are reported through SdkError and a zero handle', () => {
  const { call, callString } = loadPlugin({ withBundle: true });
  assert.equal(call('AurixWebGL_Create', '{not json'), 0);
  assert.notEqual(callString('AurixWebGL_SdkError'), '');
  assert.equal(call('AurixWebGL_SdkStatus'), 2, 'the SDK itself is still usable');
});

test('returned strings are NUL-terminated malloc copies the C# marshaller can read', () => {
  const { call, env } = loadPlugin({ withBundle: true });
  const handle = call('AurixWebGL_Create', options);
  const ptr = call('AurixWebGL_Invoke', handle, 'connectionState', '{}', 0);
  assert.ok(ptr > 0);
  const text = env.UTF8ToString(ptr);
  assert.equal(env.HEAPU8[ptr + env.lengthBytesUTF8(text)], 0);
  assert.equal(text, '{"ok":true,"value":"disconnected"}');
});
