// Builds the single-file browser bundle `dist/aurix-web-sdk.js`: the SDK compiled as AMD modules
// wrapped in a tiny loader, exposed as `window.AurixWebSdk` (and `module.exports` under CommonJS).
// No bundler dependency: only `tsc`. Used by the Unity WebGL plugin (`sdk/unity/Runtime/WebGL`)
// and by plain `<script>` pages that do not ship an ES-module toolchain.
// Usage: node scripts/bundle.mjs
import { execFileSync } from 'node:child_process';
import { readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const require = createRequire(import.meta.url);
const pkg = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
const amdFile = join(root, 'dist', 'aurix-web-sdk.amd.js');
const outFile = join(root, 'dist', 'aurix-web-sdk.js');

execFileSync(process.execPath, [require.resolve('typescript/bin/tsc'), '-p', 'tsconfig.bundle.json'], {
  cwd: root,
  stdio: 'inherit',
});
const amd = readFileSync(amdFile, 'utf8');
rmSync(amdFile, { force: true });

const banner = `/*! ${pkg.name} v${pkg.version} — browser bundle (window.AurixWebSdk). ${pkg.license ?? ''} */`;
const wrapped = `${banner}
(function (root, factory) {
  var api = factory();
  if (typeof module === 'object' && module && module.exports) module.exports = api;
  root.AurixWebSdk = api;
})(typeof globalThis !== 'undefined' ? globalThis : typeof self !== 'undefined' ? self : this, function () {
  'use strict';
  var modules = Object.create(null);
  var cache = Object.create(null);
  function define(name, deps, factory) {
    modules[name] = { deps: deps, factory: factory };
  }
  function load(name) {
    var cached = cache[name];
    if (cached) return cached.exports;
    var m = modules[name];
    if (!m) throw new Error('aurix-web-sdk: module not found: ' + name);
    var mod = { exports: {} };
    cache[name] = mod;
    var args = m.deps.map(function (dep) {
      if (dep === 'require') return load;
      if (dep === 'exports') return mod.exports;
      return load(dep);
    });
    var ret = m.factory.apply(null, args);
    if (ret !== undefined) mod.exports = ret;
    return mod.exports;
  }
${amd}
  var api = load('index');
  try {
    Object.defineProperty(api, 'version', { value: ${JSON.stringify(pkg.version)}, enumerable: true });
  } catch (e) {
    // frozen namespace: version is optional
  }
  return api;
});
`;
writeFileSync(outFile, wrapped);
console.log(`wrote ${outFile} (${(Buffer.byteLength(wrapped) / 1024).toFixed(0)} KiB)`);
