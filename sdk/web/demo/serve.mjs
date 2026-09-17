// Tiny static server for the demo: serves ./demo and ../dist without any dependency.
// Usage: node demo/serve.mjs [port]   (default 5173)
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const port = Number(process.argv[2] ?? 5173);
const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.map': 'application/json', '.css': 'text/css' };

createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', 'http://localhost');
  let path = url.pathname === '/' ? '/demo/index.html' : url.pathname;
  path = normalize(path).replace(/^(\.\.[/\\])+/, '');
  const file = join(root, path);
  if (!file.startsWith(root)) { res.writeHead(403).end(); return; }
  try {
    const body = await readFile(file);
    res.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404).end('not found');
  }
}).listen(port, () => console.log(`Aurix web demo: http://localhost:${port}/`));
