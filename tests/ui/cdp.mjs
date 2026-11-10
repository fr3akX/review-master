// Minimal Chrome DevTools driver, no dependencies: node cdp.mjs <url> <suite.mjs> [--dark].
// The suite default-exports async (page) => {}; a thrown error, a page exception or a
// console.error fails the run. Screenshots go to $SHOTS.
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [url, scriptPath, ...rest] = process.argv.slice(2);
const dark = rest.includes('--dark');
const profile = mkdtempSync(join(tmpdir(), 'rm-chrome-'));
const chrome = spawn(process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', [
  '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run', '--disable-gpu',
  '--window-size=1500,950', 'about:blank',
], { stdio: ['ignore', 'ignore', 'pipe'] });
const wsUrl = await new Promise((ok, bad) => {
  let buf = '';
  chrome.stderr.on('data', d => { buf += d; const m = /DevTools listening on (ws:\/\/\S+)/.exec(buf); if (m) ok(m[1]); });
  setTimeout(() => bad(new Error('chrome did not start: ' + buf)), 15000);
});
const port = new URL(wsUrl).port;
const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const ws = new WebSocket(targets.find(t => t.type === 'page').webSocketDebuggerUrl);
await new Promise(r => (ws.onopen = r));
let seq = 0; const pending = new Map(); const logs = [];
ws.onmessage = ev => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { const { ok, bad } = pending.get(m.id); pending.delete(m.id); m.error ? bad(new Error(m.error.message)) : ok(m.result); }
  else if (m.method === 'Runtime.consoleAPICalled') logs.push(`[console.${m.params.type}] ` + m.params.args.map(a => a.value ?? a.description).join(' '));
  else if (m.method === 'Runtime.exceptionThrown') logs.push('[exception] ' + (m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text));
};
const send = (method, params = {}) => new Promise((ok, bad) => { const id = ++seq; pending.set(id, { ok, bad }); ws.send(JSON.stringify({ id, method, params })); });
const sleep = ms => new Promise(r => setTimeout(r, ms));
await send('Runtime.enable'); await send('Page.enable');
await send('Emulation.setDeviceMetricsOverride', { width: 1500, height: 950, deviceScaleFactor: 1, mobile: false });
if (dark) await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] });
const page = {
  logs, sleep, send, url,
  async goto(u) { await send('Page.navigate', { url: u }); await sleep(400); },
  async eval(fn, ...args) {
    const expr = `(${fn})(...${JSON.stringify(args)})`;
    const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error('eval: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text));
    return r.result.value;
  },
  async waitFor(fn, ms = 8000, ...args) {
    const t0 = Date.now();
    for (;;) { const v = await page.eval(fn, ...args); if (v) return v; if (Date.now() - t0 > ms) throw new Error('waitFor timed out: ' + fn); await sleep(100); }
  },
  async shot(name) {
    const r = await send('Page.captureScreenshot', { format: 'png' });
    const file = resolve(process.env.SHOTS || '.', name + '.png');
    writeFileSync(file, Buffer.from(r.data, 'base64'));
    return file;
  },
  async key(key, opts = {}) {
    const base = { key, code: opts.code || (key.length === 1 ? 'Key' + key.toUpperCase() : key), text: key.length === 1 ? key : undefined, modifiers: opts.modifiers || 0, windowsVirtualKeyCode: opts.vk || (key.length === 1 ? key.toUpperCase().charCodeAt(0) : 0) };
    await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    await sleep(80);
  },
  async click(x, y, opts = {}) {
    const b = { x, y, button: 'left', clickCount: 1, modifiers: opts.modifiers || 0 };
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', ...b });
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', ...b });
    await sleep(120);
  },
  async clickSel(sel, opts = {}) {
    const r = await page.eval(s => { const el = document.querySelector(s); if (!el) return null; el.scrollIntoView({ block: 'center' }); const b = el.getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; }, sel);
    if (!r) throw new Error('no element ' + sel);
    await page.click(r.x, r.y, opts);
  },
};
let code = 0;
try {
  await page.goto(url);
  const mod = await import(pathToFileURL(resolve(scriptPath)).href);
  await mod.default(page);
} catch (e) { console.error('FAIL:', e.message); code = 1; }
const bad = logs.filter(l => /exception|console\.error/.test(l));
if (logs.length) console.log(logs.join('\n'));
if (bad.length) code = 1;
ws.close(); chrome.kill();
process.exit(code);
