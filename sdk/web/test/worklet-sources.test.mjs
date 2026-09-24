// SPDX-FileCopyrightText: 2026 The Aurix Authors
// SPDX-License-Identifier: Apache-2.0
import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { FakeAudioWorkletProcessor } from './helpers/fake-audio.mjs';

const dist = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'dist');

/**
 * Bundlers minify the SDK together with the page, renaming module-level functions and classes.
 * The worklet / worker sources are built from `Function.prototype.toString()`, so nothing in them
 * may refer to a module-level binding by its literal name inside a string. Simulate the mangler on
 * the compiled module: rename every declared function / class in code, leaving string and template
 * literal text alone (like a real minifier), turn every class declaration into an anonymous class
 * expression (`X = class {}`, as rolldown/oxc emit — its `toString()` carries no name), then
 * import the mangled module and run its source.
 */
function mangleModule(code) {
  const names = new Map(
    [...code.matchAll(/\b(?:function|class)\s+([A-Za-z_$][\w$]*)/g)].map((m, i) => [m[1], `_m${i}`]),
  );
  assert.ok(names.size > 0, 'module declares at least one unit');
  let out = '';
  let i = 0;
  const templateDepth = []; // brace depth inside each open `${ … }` of nested template literals
  const copyString = (quote) => {
    const start = i++;
    while (i < code.length && code[i] !== quote) i += code[i] === '\\' ? 2 : 1;
    out += code.slice(start, ++i);
  };
  const copyTemplateText = () => {
    // inside a template literal: copy verbatim up to the closing backtick or the next `${`
    for (;;) {
      if (code[i] === '\\') {
        out += code.slice(i, i + 2);
        i += 2;
      } else if (code[i] === '`') {
        out += code[i++];
        return;
      } else if (code.startsWith('${', i)) {
        out += '${';
        i += 2;
        templateDepth.push(0);
        return;
      } else out += code[i++];
    }
  };
  while (i < code.length) {
    const c = code[i];
    if (c === '/' && code[i + 1] === '/') {
      const end = code.indexOf('\n', i);
      out += code.slice(i, end < 0 ? code.length : end);
      i = end < 0 ? code.length : end;
    } else if (c === '/' && code[i + 1] === '*') {
      const end = code.indexOf('*/', i) + 2;
      out += code.slice(i, end);
      i = end;
    } else if (c === "'" || c === '"') copyString(c);
    else if (c === '`') {
      out += code[i++];
      copyTemplateText();
    } else if (templateDepth.length > 0 && c === '}' && templateDepth.at(-1) === 0) {
      out += code[i++];
      templateDepth.pop();
      copyTemplateText();
    } else if (/[A-Za-z_$]/.test(c)) {
      const m = /^[A-Za-z_$][\w$]*/.exec(code.slice(i))[0];
      out += names.get(m) ?? m;
      i += m.length;
    } else {
      if (templateDepth.length > 0) {
        if (c === '{') templateDepth[templateDepth.length - 1] += 1;
        else if (c === '}') templateDepth[templateDepth.length - 1] -= 1;
      }
      out += code[i++];
    }
  }
  out = out.replace(/\b(export\s+)?class\s+([A-Za-z_$][\w$]*)\s*(extends\s+[\w$.]+\s*)?\{/g, (m, ex, name, ext) =>
    name === 'extends' ? m : `${ex ?? ''}var ${name} = class ${ext ?? ''}{`,
  );
  return { code: out, names };
}

async function mangled(file, exportName) {
  const tmp = join(dist, `.mangled-${file}`);
  const { code, names } = mangleModule(readFileSync(join(dist, file), 'utf8'));
  writeFileSync(tmp, code);
  try {
    const mod = await import(`${tmp}?${Date.now()}`);
    const fn = mod[names.get(exportName) ?? exportName];
    assert.equal(typeof fn, 'function', `${exportName} exported (mangled to ${names.get(exportName)})`);
    return fn;
  } finally {
    rmSync(tmp, { force: true });
  }
}

function workletScope() {
  const registered = new Map();
  const scope = {
    sampleRate: 48000,
    currentTime: 0,
    AudioWorkletProcessor: FakeAudioWorkletProcessor,
    registerProcessor: (name, cls) => registered.set(name, cls),
    Float32Array,
    Int16Array,
    Uint8Array,
    Math,
    Number,
    Map,
    Set,
    Array,
    Error,
    console,
  };
  scope.globalThis = scope;
  return { scope, registered };
}

for (const [label, file, exportName, expected] of [
  ['AURX playback/capture worklet', 'aurx-audio.js', 'aurxWorkletSource', ['aurix-aurx-player', 'aurix-aurx-capture']],
  ['voice-effects worklet', 'effects.js', 'voiceEffectsWorkletSource', ['aurix-voice-effects']],
  ['viseme worklet', 'visemes.js', 'visemeWorkletSource', ['aurix-visemes']],
]) {
  test(`${label} source survives minification of the SDK module`, async () => {
    const make = await mangled(file, exportName);
    const { scope, registered } = workletScope();
    vm.runInNewContext(make(), scope, { filename: `${label}.js` });
    for (const name of expected) assert.ok(registered.get(name), `${name} registered`);
  });
}

test('E2EE worker source survives minification of the SDK module', async () => {
  const make = await mangled('e2ee.js', 'e2eeWorkerSource');
  const self = { crypto: globalThis.crypto, Uint8Array, DataView, Map, Set, Error, console };
  self.self = self;
  self.globalThis = self;
  vm.runInNewContext(make(), self, { filename: 'e2ee-worker.js' });
  assert.equal(typeof self.onmessage, 'function', 'worker installed its message handler');
});
