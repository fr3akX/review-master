'use strict';
// review-master browser UI. Vanilla JS against the /api contract served by the
// local backend; no framework, no build step, no external requests.

// ───────────────────────────── constants & state ─────────────────────────────

const SEVERITIES = ['blocker', 'major', 'minor', 'nit'];
const EVENTS = [
  { value: 'comment', label: 'Comment', hint: 'General feedback without explicit approval.' },
  { value: 'approve', label: 'Approve', hint: 'Approve merging these changes.' },
  { value: 'request_changes', label: 'Request changes', hint: 'Feedback that must be addressed before merging.' },
];
const STATUS_LETTER = { added: 'A', modified: 'M', deleted: 'D', renamed: 'R' };
const MARK = { add: '+', del: '−', ctx: '' };
const HL_LINE_LIMIT = 8000; // files with more lines than this are shown unhighlighted
const POLL_MS = 700;

// File extension (or hljs alias) → highlight.js language name.
const LANGS = {
  rs: 'rust', py: 'python', pyi: 'python', ts: 'typescript', tsx: 'typescript', mts: 'typescript', cts: 'typescript',
  js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript', go: 'go', java: 'java',
  kt: 'kotlin', kts: 'kotlin', scala: 'scala', sc: 'scala', sbt: 'scala', groovy: 'groovy', gradle: 'gradle',
  c: 'c', h: 'c', cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hh: 'cpp', hxx: 'cpp', cs: 'csharp',
  rb: 'ruby', php: 'php', swift: 'swift', m: 'objectivec', mm: 'objectivec', lua: 'lua', pl: 'perl', r: 'r',
  md: 'markdown', markdown: 'markdown', toml: 'ini', ini: 'ini', cfg: 'ini', yaml: 'yaml', yml: 'yaml',
  json: 'json', jsonc: 'json', sh: 'bash', bash: 'bash', zsh: 'bash', sql: 'sql', proto: 'protobuf',
  xml: 'xml', html: 'xml', htm: 'xml', svg: 'xml', vue: 'xml', css: 'css', scss: 'scss', less: 'less',
  graphql: 'graphql', gql: 'graphql', diff: 'diff', patch: 'diff', ps1: 'powershell', mk: 'makefile',
  dockerfile: 'dockerfile', makefile: 'makefile',
};

const S = {
  info: null,
  files: new Map(), // path → FileState
  order: [], // FileState[] in PR order
  drafts: [],
  threads: [],
  current: null, // FileState highlighted in the sidebar
  collapsed: new Set(), // sidebar directory paths that are folded
  sources: new Map(), // path → /api/source response (file viewer cache)
  viewer: null, // { path, fs, pane } while the file viewer is open
  sel: null, // { pane, side, from, start, line, marked, snippet }
  dragging: false,
  pop: null, // { el, label, comment }
  scope: { tests: false, dirs: new Set(), extra: new Set() }, // exclusion controls (see "review scope")
  excluded: new Set(), // changed-file paths excluded from the review (server state)
  allPaths: new Set(), // every changed path, in scope or excluded
  fileCache: new Map(), // path → FileState, reused across scope changes
  scopeChain: Promise.resolve(), // serializes POST /api/scope
  scopePop: null, // open "N excluded" restore popover
  io: null, // IntersectionObserver that lazily loads diffs
  sym: null, // { el } — open symbol (LSP) popup
  foldOverrides: new Map(), // "path|foldKey" → open: per-fold choices since the last tests-toggle flip (persisted)
  foldCache: new Map(), // path → Promise<ranges> from /api/folds
  summary: { text: null, act: null, running: false, error: null, loadFailed: false, collapsed: false }, // PR summary card
  lspWarm: new Set(), // paths whose language server was asked to start
  composers: new Map(), // key → { key, path, akey, el, ta, inViewer }
  drawer: null, // 'chat' | 'review' | null
  chat: { threadId: null, anchor: null, snippet: null, thread: null, jobs: new Map() }, // jobs: threadId → ChatJob
  review: { body: '', event: 'comment', submitting: false, list: null },
};

// ───────────────────────────── small helpers ─────────────────────────────

const $ = (sel, root = document) => root.querySelector(sel);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const esc = s => String(s).replace(/[&<>"']/g, c => ESC[c]);

// Element factory: h('div', {class, onclick, html, ...attrs}, ...children).
function h(tag, props, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'html') el.innerHTML = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (v === true) el[k] = true;
    else el.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid != null && kid !== false) el.append(kid instanceof Node ? kid : String(kid));
  }
  return el;
}

function toast(msg, kind = 'info', ms = 6000) {
  const t = h('div', { class: `toast ${kind}`, role: 'status', onclick: () => t.remove() }, msg);
  $('#toasts').append(t);
  setTimeout(() => t.classList.add('out'), ms);
  setTimeout(() => t.remove(), ms + 400);
}

// Every API failure is toasted once here; the thrown error is marked so the
// global unhandledrejection handler does not toast it again.
// quiet: no toast (callers that show the failure themselves, e.g. LSP lookups).
async function api(method, url, body, quiet = false) {
  const init = { method, headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(url, init);
  } catch (e) {
    throw failed(`Network error (${method} ${url}): ${e.message}`, quiet);
  }
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!res.ok) throw failed((data && data.error) || `${method} ${url} failed: HTTP ${res.status}`, quiet);
  return data;
}
function failed(msg, quiet = false) {
  if (!quiet) toast(msg, 'error');
  const e = new Error(msg);
  e.toasted = true;
  return e;
}
const get = url => api('GET', url);
const post = (url, body) => api('POST', url, body);
const qpath = path => encodeURIComponent(path);

// Shared job poller (chat turns, PR summary): streams progress lines, resolves with result.
// quiet: poll failures are not toasted (the caller shows them).
async function pollJob(id, onProgress, quiet = false) {
  let since = 0;
  for (;;) {
    const j = await api('GET', `/api/job/${id}?since=${since}`, undefined, quiet);
    if (j.progress && j.progress.length) onProgress(j.progress);
    since = j.next;
    if (j.done) {
      if (j.error) throw new Error(j.error);
      return j.result || {};
    }
    await sleep(POLL_MS);
  }
}

function lineLabel({ side, start, line }) {
  const range = start != null && start !== line ? `L${start}–${line}` : `L${line}`;
  return side === 'LEFT' ? `${range} (old)` : range;
}
const anchorText = a => (a ? `${a.path}:${lineLabel({ side: a.side, start: a.start, line: a.line })}` : 'Whole PR');
const anchorKey = (side, line) => `${side || 'RIGHT'}:${line}`;

function splitPath(path) {
  const i = path.lastIndexOf('/');
  return i < 0 ? ['', path] : [path.slice(0, i + 1), path.slice(i + 1)];
}
function pathLabel(path) {
  const [dir, base] = splitPath(path);
  return h('span', { class: 'path' }, dir && h('span', { class: 'dir' }, dir), h('span', { class: 'base' }, base));
}
const statusBadge = status => h('span', { class: `status st-${status}`, title: status }, STATUS_LETTER[status] || '?');
const counts = f => h('span', { class: 'counts' }, h('span', { class: 'plus' }, `+${f.additions}`), h('span', { class: 'minus' }, `−${f.deletions}`));
const sevPill = sev => h('span', { class: `sev sev-${sev}` }, sev);

function flash(el) {
  el.classList.remove('flash');
  void el.offsetWidth; // restart the animation
  el.classList.add('flash');
}

// Textareas grow with content; CSS field-sizing handles it where supported.
function autosize(ta) {
  if (window.CSS && CSS.supports('field-sizing', 'content')) return;
  const fit = () => {
    ta.style.height = 'auto';
    ta.style.height = `${ta.scrollHeight + 2}px`;
  };
  ta.addEventListener('input', fit);
  requestAnimationFrame(fit);
}

function onCtrlEnter(ta, fn) {
  ta.addEventListener('keydown', e => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      fn();
    }
  });
}

// ───────────────────────────── syntax highlighting ─────────────────────────────

function hlLang(name) {
  if (!name || !window.hljs) return null;
  const n = LANGS[name.toLowerCase()] || name.toLowerCase();
  return hljs.getLanguage(n) ? n : null;
}

function langForPath(path) {
  const base = splitPath(path)[1].toLowerCase();
  if (base === 'dockerfile' || base.startsWith('dockerfile.')) return hlLang('dockerfile');
  if (base === 'makefile' || base === 'gnumakefile') return hlLang('makefile');
  const dot = base.lastIndexOf('.');
  return dot > 0 ? hlLang(base.slice(dot + 1)) : null;
}

function hlCode(code, lang) {
  if (!lang) return esc(code);
  try {
    return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value;
  } catch {
    return esc(code);
  }
}

// Highlight a whole text once and split into per-line HTML, closing spans at
// each newline and reopening them on the next line so multi-line tokens stay intact.
function hlLines(lines, lang) {
  if (!lang) return lines.map(esc);
  try {
    const out = splitHighlighted(hljs.highlight(lines.join('\n'), { language: lang, ignoreIllegals: true }).value);
    if (out.length === lines.length) return out;
  } catch {
    /* fall through */
  }
  return lines.map(esc);
}

function splitHighlighted(html) {
  const out = [];
  const open = [];
  let cur = '';
  const re = /(<span[^>]*>)|(<\/span>)|(\n)|([^<\n]+)/g;
  let m;
  while ((m = re.exec(html))) {
    if (m[1]) {
      open.push(m[1]);
      cur += m[1];
    } else if (m[2]) {
      open.pop();
      cur += m[2];
    } else if (m[3]) {
      out.push(cur + '</span>'.repeat(open.length));
      cur = open.join('');
    } else cur += m[4];
  }
  out.push(cur);
  return out;
}

// ───────────────────────────── markdown ─────────────────────────────
// Minimal and safe: everything is HTML-escaped; only the constructs below are emitted.

const BLOCK_START = /^\s*(```|~~~|>|#{1,6}\s|[-*+]\s|\d+[.)]\s)/;
const LIST_ITEM = /^\s*([-*+]|\d+[.)])\s+(.*)$/;

// headings: false renders "# x" lines as plain paragraphs (LSP hover text).
function md(src, { headings = true } = {}) {
  const lines = String(src ?? '').replace(/\r\n?/g, '\n').split('\n');
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    let m;
    if ((m = line.match(/^\s*(```+|~~~+)\s*([\w+#.-]*)/))) {
      const fence = m[1];
      const body = [];
      i++;
      while (i < lines.length && !lines[i].trimStart().startsWith(fence)) body.push(lines[i++]);
      i++; // closing fence
      out.push(`<pre class="md-code"><code>${hlCode(body.join('\n'), hlLang(m[2]))}</code></pre>`);
    } else if (!line.trim()) {
      i++;
    } else if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      const lvl = Math.min(m[1].length, 3) + 2; // # → h3 … keeps headings modest inside cards
      out.push(headings ? `<h${lvl}>${inline(m[2])}</h${lvl}>` : `<p>${inline(m[2])}</p>`);
      i++;
    } else if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*> ?/, ''));
      out.push(`<blockquote>${md(quote.join('\n'), { headings })}</blockquote>`);
    } else if ((m = line.match(LIST_ITEM))) {
      const ordered = /\d/.test(m[1]);
      const items = [];
      while (i < lines.length) {
        const it = lines[i].match(LIST_ITEM);
        if (it && /\d/.test(it[1]) === ordered) items.push(it[2]);
        else if (items.length && /^\s{2,}\S/.test(lines[i])) items[items.length - 1] += ` ${lines[i].trim()}`;
        else break;
        i++;
      }
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>${items.map(t => `<li>${inline(t)}</li>`).join('')}</${tag}>`);
    } else {
      const para = [];
      do para.push(lines[i++]);
      while (i < lines.length && lines[i].trim() && !BLOCK_START.test(lines[i]));
      out.push(`<p>${para.map(inline).join('<br>')}</p>`);
    }
  }
  return out.join('');
}

// Code references like `src/a.rs:12` or `main.go:3-9`: a path with a directory or an
// extension, then :N or :N-M. Matched on already-escaped text.
const CODE_REF = /(?<![\w/.-])((?:[\w.-]+\/)+[\w.-]+|[\w.-]*\w\.[A-Za-z]\w*):(\d+)(?:[-–](\d+))?(?!\w)/g;
const linkRefs = (html, wrap = x => x) => html.replace(CODE_REF, (m, path, a, b) =>
  wrap(`<a class="code-ref" href="#" data-path="${path}" data-line="${a}"${b ? ` data-end="${b}"` : ''} title="Open ${path}">${m}</a>`));

function inline(text) {
  const slots = [];
  const keep = html => `\u0000${slots.push(html) - 1}\u0000`;
  let s = String(text).replace(/`([^`]+)`/g, (_, code) => keep(`<code>${linkRefs(esc(code))}</code>`));
  s = esc(s).replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, (_, t, url) =>
    keep(`<a href="${url}" target="_blank" rel="noopener noreferrer">${t}</a>`));
  s = linkRefs(s, keep)
    .replace(/\*\*(?=\S)(.+?)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^\w*])\*(?=[^\s*])([^*]*?[^\s*])?\*(?![\w*])/g, (all, pre, body) =>
      (body === undefined ? all : `${pre}<em>${body}</em>`));
  // Slots can nest (a code span inside link text), so restore until stable.
  for (let n = 0; n < 3 && s.includes('\u0000'); n++) s = s.replace(/\u0000(\d+)\u0000/g, (_, k) => slots[k]);
  return s;
}

// ───────────────────────────── header, banner, sidebar ─────────────────────────────

function renderHeader() {
  const { pr, mode, branch, head, engine } = S.info;
  const title = $('#pr-title');
  const meta = $('#pr-meta');
  if (pr) {
    title.replaceChildren(
      h('a', { href: pr.url, target: '_blank', rel: 'noopener noreferrer' }, pr.title),
      h('span', { class: 'pr-number' }, ` #${pr.number}`),
    );
    const state = pr.merged ? 'merged' : pr.draft ? 'draft' : String(pr.state || '').toLowerCase();
    meta.replaceChildren(
      h('span', { class: `state state-${state}` }, state),
      h('span', {}, h('strong', {}, pr.author), ' wants ', h('code', {}, pr.base_ref), ' ← ', h('code', {}, pr.head_ref)),
    );
    document.title = `#${pr.number} ${pr.title} · review-master`;
  } else {
    title.replaceChildren(h('span', {}, `Local review · ${branch}`));
    meta.replaceChildren(
      h('span', { class: 'state state-local' }, mode),
      h('span', {}, h('code', {}, branch), ' @ ', h('code', {}, String(head || '').slice(0, 10))),
    );
    document.title = `${branch} · review-master`;
  }
  $('#engine-chip').textContent = engine ? `${engine.kind}${engine.model ? ` · ${engine.model}` : ''}` : 'no engine';
}

function submittedNode() {
  const url = S.info.submitted;
  return h('div', { class: 'banner ok' },
    h('strong', {}, 'Review submitted'), ' — ',
    /^https?:\/\//.test(url) ? h('a', { href: url, target: '_blank', rel: 'noopener noreferrer' }, url) : url);
}

function renderBanner() {
  const b = $('#banner');
  b.hidden = !S.info.submitted;
  b.replaceChildren(S.info.submitted ? submittedNode() : '');
}

// Review badge counts drafts that will be submitted (not those on excluded files).
function renderDraftCounts() {
  const n = S.drafts.filter(d => !S.excluded.has(d.path)).length;
  const badge = $('#draft-count');
  badge.textContent = n;
  badge.hidden = n === 0;
  const per = new Map();
  for (const d of S.drafts) per.set(d.path, (per.get(d.path) || 0) + 1);
  for (const fs of S.order) {
    const c = per.get(fs.path) || 0;
    fs.navBadge.textContent = c;
    fs.navBadge.hidden = c === 0;
  }
}

// ───────────── review scope: excluded files ─────────────
// Excluding takes changed files out of the review on the server (POST /api/scope):
// no sections or tree rows, not in chat context, drafts on them are not submitted.
// The server owns the excluded set; locally we only remember which controls
// (tests toggle, excluded directories) produced it, and re-derive them on load.

const TEST_DIRS = new Set(['tests', 'test', '__tests__', 'testdata', 'testing', 'spec', 'fixtures']);
const TEST_BASE = /(^test_.+\..+$)|(^.+_tests?\..+$)|(^.+_spec\..+$)|(^.+\.(test|spec)\..+$)|(^conftest\.py$)/;
const EYE_OFF = '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true">' +
  '<path d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/><path d="M2.5 13.5l11-11"/></svg>';

function isTestPath(path) {
  const parts = path.toLowerCase().split('/');
  const base = parts.pop();
  return parts.some(p => TEST_DIRS.has(p)) || TEST_BASE.test(base);
}

const underDir = (path, dir) => path.startsWith(dir);
const pathsUnder = dir => [...S.allPaths].filter(p => underDir(p, dir));
const testPaths = () => [...S.allPaths].filter(isTestPath);
const coveredByControls = p => (S.scope.tests && isTestPath(p)) || [...S.scope.dirs].some(d => underDir(p, d));

const prId = () => (S.info.pr ? `${S.info.pr.owner}/${S.info.pr.repo}#${S.info.pr.number}` : `local:${S.info.branch}`);

function scopePrefKey() {
  return `review-master:scope:${prId()}`;
}

function saveScopePrefs() {
  try {
    localStorage.setItem(scopePrefKey(), JSON.stringify({ tests: S.scope.tests, dirs: [...S.scope.dirs] }));
  } catch {
    /* storage unavailable: controls fall back to single-file entries on reload */
  }
}

// Re-derives the controls from the server's excluded set: a remembered control
// holds only while everything it covers is still excluded; the rest are single files.
function reconcileScope() {
  let prefs = {};
  try {
    prefs = JSON.parse(localStorage.getItem(scopePrefKey()) || '{}') || {};
  } catch {
    prefs = {};
  }
  const allOut = paths => paths.length > 0 && paths.every(p => S.excluded.has(p));
  S.scope.tests = !!prefs.tests && allOut(testPaths());
  const dirs = Array.isArray(prefs.dirs) ? prefs.dirs : [];
  S.scope.dirs = new Set(dirs.filter(d => typeof d === 'string' && allOut(pathsUnder(d))));
  S.scope.extra = new Set([...S.excluded].filter(p => !coveredByControls(p)));
}

function computeExcluded() {
  const out = new Set(S.scope.extra);
  if (S.scope.tests) for (const p of testPaths()) out.add(p);
  for (const d of S.scope.dirs) for (const p of pathsUnder(d)) out.add(p);
  return [...out].sort();
}

// Applies a control mutation on the server (one request at a time) and re-renders
// from the returned file list; reverts the controls when the request fails.
function changeScope(mutate, message = null) {
  const run = S.scopeChain.then(async () => {
    const prev = { tests: S.scope.tests, dirs: new Set(S.scope.dirs), extra: new Set(S.scope.extra) };
    mutate();
    let r;
    try {
      r = await post('/api/scope', { excluded: computeExcluded() });
    } catch {
      Object.assign(S.scope, prev);
      renderScopeControls();
      return false;
    }
    S.excluded = new Set(r.excluded || []);
    for (const f of r.files || []) S.allPaths.add(f.path);
    for (const p of S.excluded) S.allPaths.add(p);
    S.scope.extra = new Set([...S.scope.extra].filter(p => S.excluded.has(p)));
    saveScopePrefs();
    renderScope(r.files || []);
    if (prev.tests !== S.scope.tests) resetTestFolds();
    if (message) toast(typeof message === 'function' ? message() : message, 'info');
    return true;
  });
  S.scopeChain = run.catch(() => false);
  return run;
}

const setExcludeTests = on => changeScope(() => { S.scope.tests = on; });
const restoreBranch = dir => changeScope(() => S.scope.dirs.delete(dir), `Restored ${dir}`);
const restoreFile = path => changeScope(() => S.scope.extra.delete(path), `Restored ${path}`);
const excludeFile = path => changeScope(() => S.scope.extra.add(path));
const restoreAll = () => changeScope(() => {
  S.scope.tests = false;
  S.scope.dirs.clear();
  S.scope.extra.clear();
}, 'Restored all excluded files');

function excludeBranch(dir) {
  return changeScope(() => {
    for (const d of [...S.scope.dirs]) if (d.startsWith(dir)) S.scope.dirs.delete(d); // subsumed by the new one
    S.scope.dirs.add(dir);
  });
}

// A jump into an excluded file first brings it back: its excluded directory, the
// tests toggle, or the file itself. Resolves false when the restore failed.
function ensureInScope(path) {
  if (!S.excluded.has(path)) return Promise.resolve(true);
  const what = [];
  return changeScope(() => {
    for (const d of [...S.scope.dirs]) {
      if (underDir(path, d)) {
        S.scope.dirs.delete(d);
        what.push(d);
      }
    }
    if (S.scope.tests && isTestPath(path)) {
      S.scope.tests = false;
      what.push('test files');
    }
    if (S.scope.extra.delete(path) || !what.length) what.push(path);
  }, () => `Restored ${what.join(', ')}`);
}

function renderScopeControls() {
  const nTests = testPaths().length;
  $('#hide-tests').checked = S.scope.tests;
  $('#test-count').textContent = `Exclude tests · ${nTests}`;
  $('#test-toggle').hidden = nTests === 0;
  if (S.scopePop) renderScopePop();
}

function toggleScopePop() {
  if (S.scopePop) return closeScopePop();
  S.scopePop = h('div', { class: 'scope-pop' });
  $('#sidebar .side-head').append(S.scopePop);
  renderScopePop();
}

function closeScopePop() {
  if (!S.scopePop) return;
  S.scopePop.remove();
  S.scopePop = null;
}

function renderScopePop() {
  const row = (label, restore) => h('li', {},
    h('span', { class: 'scope-label', title: label }, label),
    h('button', { class: 'icon-btn', title: 'Restore to the review', 'aria-label': `Restore ${label}`, onclick: restore }, '✕'));
  const rows = [];
  if (S.scope.tests) rows.push(row(`Tests (${testPaths().length})`, () => setExcludeTests(false)));
  for (const d of [...S.scope.dirs].sort()) rows.push(row(`${d} (${pathsUnder(d).length})`, () => restoreBranch(d)));
  for (const p of [...S.scope.extra].sort()) rows.push(row(p, () => restoreFile(p)));
  if (!rows.length) return closeScopePop();
  S.scopePop.replaceChildren(
    h('div', { class: 'scope-pop-title' }, 'Excluded from review'),
    h('ul', {}, rows),
    h('button', { class: 'btn small', onclick: restoreAll }, 'Show all'));
}

function buildSidebar() {
  const add = S.order.reduce((n, fs) => n + fs.file.additions, 0);
  const del = S.order.reduce((n, fs) => n + fs.file.deletions, 0);
  const out = S.excluded.size;
  $('#file-summary').replaceChildren(plural(S.order.length, 'file'), ' ', counts({ additions: add, deletions: del }),
    ...(out ? [' · ', h('button', { class: 'link-btn scope-link', title: 'Excluded files', onclick: toggleScopePop }, `${out} excluded`)] : []));
  const root = { dirs: new Map(), files: [] };
  for (const fs of S.order) {
    let node = root;
    for (const part of fs.path.split('/').slice(0, -1)) {
      if (!node.dirs.has(part)) node.dirs.set(part, { dirs: new Map(), files: [] });
      node = node.dirs.get(part);
    }
    node.files.push(fs);
  }
  $('#file-list').replaceChildren(...treeItems(root, ''));
}

// Sidebar tree items: directories first (single-child chains compressed into one
// row, e.g. "src/block_meta/"), then files; folding state lives in S.collapsed.
function treeItems(node, prefix) {
  const items = [];
  for (const [name, child] of [...node.dirs].sort((a, b) => a[0].localeCompare(b[0]))) {
    let label = `${name}/`;
    let dir = child;
    while (dir.dirs.size === 1 && !dir.files.length) {
      const [[next, sub]] = dir.dirs;
      label += `${next}/`;
      dir = sub;
    }
    const path = prefix + label;
    const li = h('li', { class: `tree-dir${S.collapsed.has(path) ? ' collapsed' : ''}` });
    const toggle = () => {
      const closed = !S.collapsed.delete(path);
      if (closed) S.collapsed.add(path);
      li.classList.toggle('collapsed', closed);
    };
    li.append(
      h('div', { class: 'dir-line' },
        h('button', { class: 'dir-row', title: path, onclick: toggle }, h('span', { class: 'chevron' }, '▾'), h('span', { class: 'dir-name' }, label)),
        h('button', { class: 'row-hide', title: 'Exclude from review', 'aria-label': `Exclude ${path} from the review`, onclick: () => excludeBranch(path), html: EYE_OFF })),
      h('ul', { class: 'tree' }, treeItems(dir, path)));
    items.push(li);
  }
  for (const fs of [...node.files].sort((a, b) => a.path.localeCompare(b.path))) items.push(fs.nav);
  return items;
}

function setCurrent(fs) {
  if (!fs || S.current === fs) return;
  if (S.current) S.current.nav.classList.remove('active');
  S.current = fs;
  fs.nav.classList.add('active');
  fs.nav.scrollIntoView({ block: 'nearest' });
}

// Highlights the last file section whose top has scrolled past the top of the main column.
function trackCurrent() {
  const top = $('#main').getBoundingClientRect().top + 60;
  let cur = null;
  for (const fs of S.order) {
    if (cur && fs.section.getBoundingClientRect().top > top) break;
    cur = fs;
  }
  setCurrent(cur);
}

function jumpToFile(fs) {
  closeViewer();
  fs.section.scrollIntoView({ block: 'start' });
  setCurrent(fs);
  ensureLoaded(fs).catch(() => {});
}

// ───────────────────────────── file sections ─────────────────────────────

function makeBox() {
  const drafts = h('div', { class: 'box-drafts' });
  const chips = h('div', { class: 'box-chips' });
  const composers = h('div', { class: 'box-composers' });
  return { root: h('div', { class: 'annot-box' }, drafts, chips, composers), drafts, chips, composers };
}
const boxEmpty = b => !b.drafts.childElementCount && !b.chips.childElementCount && !b.composers.childElementCount;

// Box for comments whose anchor row is not part of the rendered table.
function makeNotes() {
  const b = makeBox();
  b.root.classList.add('file-notes');
  b.root.prepend(h('div', { class: 'notes-title' }, 'Comments on lines not shown in this view'));
  b.root.hidden = true;
  return b;
}

const fileTitle = f => (f.status === 'renamed' && f.old_path
  ? h('span', { class: 'rename' }, pathLabel(f.old_path), h('span', { class: 'arrow' }, ' → '), pathLabel(f.path))
  : pathLabel(f.path));

function makeFileState(f) {
  const fs = { file: f, path: f.path, idx: 0, diff: null, pane: null, loading: null };
  fs.navBadge = h('span', { class: 'nav-drafts', title: 'drafts', hidden: true });
  fs.nav = h('li', {},
    h('div', { class: 'file-line' },
      h('button', { class: 'file-item', title: f.path, onclick: () => jumpToFile(fs) },
        statusBadge(f.status), h('span', { class: 'path' }, h('span', { class: 'base' }, splitPath(f.path)[1])),
        fs.navBadge, counts(f)),
      h('button', { class: 'row-hide', title: 'Exclude from review', 'aria-label': `Exclude ${f.path} from the review`, onclick: () => excludeFile(f.path), html: EYE_OFF })));
  const deleted = f.status === 'deleted';
  const full = h('button', {
    class: 'btn small', disabled: deleted,
    title: deleted ? 'File was deleted; only the diff is available' : 'Open the whole file at HEAD',
    onclick: () => openViewer(f.path),
  }, 'Full file');
  fs.head = h('div', { class: 'file-head' }, statusBadge(f.status), fileTitle(f), counts(f), h('span', { class: 'spacer' }), full);
  fs.notes = makeNotes();
  fs.body = h('div', { class: 'file-body' }, h('div', { class: 'placeholder' }, 'Loading…'));
  fs.section = h('section', { class: 'file' }, fs.head, fs.notes.root, fs.body);
  return fs;
}

// (Re)builds sections and the tree for the files in review scope; file states
// (loaded diffs, panes) are reused when a file comes back into scope.
function renderScope(files) {
  closeSym();
  if (S.viewer && S.excluded.has(S.viewer.path)) closeViewer();
  if (S.sel && S.excluded.has(S.sel.pane.path)) clearSel();
  S.files = new Map();
  S.order = files.map((f, idx) => {
    let fs = S.fileCache.get(f.path);
    if (!fs) {
      fs = makeFileState(f);
      S.fileCache.set(f.path, fs);
    }
    fs.idx = idx;
    fs.section.dataset.idx = idx;
    S.files.set(f.path, fs);
    return fs;
  });
  const wrap = $('#sections');
  if (S.order.length) wrap.replaceChildren(...S.order.map(fs => fs.section));
  else wrap.replaceChildren(h('div', { class: 'empty big' }, S.excluded.size ? 'All changed files are excluded from the review.' : 'No changed files.'));
  for (const fs of S.order) if (!fs.pane) S.io.observe(fs.section);
  if (S.current && !S.files.has(S.current.path)) {
    S.current.nav.classList.remove('active');
    S.current = null;
  }
  buildSidebar();
  renderScopeControls();
  renderDraftCounts();
  renderAllAnnotations();
  renderReviewDrafts();
  trackCurrent();
}

const fileOf = el => {
  const sec = el.closest('section.file');
  return sec ? S.order[+sec.dataset.idx] : null;
};

// A pane is one rendered code table — a section's diff or the file viewer — with
// its anchor-row index ("SIDE:line" → tr) and the annotation boxes inserted into it.
// `host` is the positioned element the selection popover is placed in.
const PANES = new WeakMap(); // table element → pane

function makePane(path, kind, table, notes, host) {
  const pane = {
    path, fs: S.files.get(path) || null, kind, table, notes, host,
    cols: kind === 'diff' ? 4 : 3, rows: new Map(), boxes: new Map(),
    folds: new Map(), // key → { key, bar, label, desc, open } (#[cfg(test)] folds)
    lastReveal: null, // row most recently jumped to; keeps its fold open when folds arrive late
  };
  for (const tr of table.tBodies[0].rows) {
    const d = tr.dataset;
    if (d.old) pane.rows.set(`LEFT:${d.old}`, tr);
    if (d.new) pane.rows.set(`RIGHT:${d.new}`, tr);
  }
  PANES.set(table, pane);
  return pane;
}

const paneOf = el => {
  const table = el.closest('table.code-table');
  return table ? PANES.get(table) : null;
};

function ensureLoaded(fs) {
  if (fs.pane) return Promise.resolve();
  if (!fs.loading) fs.loading = loadDiff(fs).finally(() => { fs.loading = null; });
  return fs.loading;
}

async function loadDiff(fs) {
  fs.body.classList.add('loading');
  try {
    fs.diff ??= await get(`/api/diff?path=${qpath(fs.path)}`);
  } catch (e) {
    fs.body.replaceChildren(h('div', { class: 'load-error' }, `Could not load diff: ${e.message} `,
      h('button', { class: 'btn small', onclick: () => ensureLoaded(fs).catch(() => {}) }, 'Retry')));
    throw e;
  } finally {
    fs.body.classList.remove('loading');
  }
  fs.pane = makePane(fs.path, 'diff', diffTable(fs), fs.notes, fs.section);
  if (fs.file.status !== 'deleted') warmLsp(fs.path);
  fs.body.replaceChildren(fs.pane.table);
  renderAnnotations(fs.pane);
  if (fs.file.status !== 'deleted') attachFolds(fs.pane);
}

function diffTable(fs) {
  const hunks = fs.diff.hunks || [];
  const total = hunks.reduce((n, hk) => n + hk.lines.length, 0);
  const lang = total <= HL_LINE_LIMIT ? langForPath(fs.path) : null;
  const rows = [];
  for (const hk of hunks) {
    rows.push(`<tr class="hunk"><td class="ln"></td><td class="ln"></td><td class="mk"></td><td class="code">${esc(hk.header)}</td></tr>`);
    const m = /\+(\d+)/.exec(hk.header);
    let lastNew = m ? +m[1] - 1 : 0; // new-file line preceding the current row, for deleted-line jumps
    for (const l of hk.lines) {
      let attrs = '';
      if (l.old != null) attrs += ` data-old="${l.old}"`;
      if (l.new != null) attrs += ` data-new="${l.new}"`;
      if (l.k === 'del') attrs += ` data-after="${lastNew}"`;
      if (l.new != null) lastNew = l.new;
      rows.push(`<tr class="ln-row ${l.k}"${attrs}><td class="ln">${l.old ?? ''}</td><td class="ln">${l.new ?? ''}</td>` +
        `<td class="mk">${MARK[l.k] ?? ''}</td><td class="code">${hlCode(l.text, lang)}</td></tr>`);
    }
  }
  if (!hunks.length) rows.push('<tr class="note"><td colspan="4">No textual changes (binary file or mode change).</td></tr>');
  return h('table', {
    class: 'code-table diff',
    html: '<colgroup><col class="c-ln"><col class="c-ln"><col class="c-mk"><col></colgroup>' +
      `<tbody>${rows.join('')}</tbody>`,
  });
}

function fullTable(path, src) {
  let text = src.text || '';
  if (text.endsWith('\n')) text = text.slice(0, -1);
  const lines = text ? text.split('\n') : [];
  const lang = lines.length <= HL_LINE_LIMIT ? hlLang(src.lang) || langForPath(path) : null;
  const html = hlLines(lines, lang);
  const added = new Set(src.added || []);
  const delAfter = src.deleted_after || {};
  const marker = n => {
    const c = +(delAfter[n] || 0);
    return c > 0
      ? `<tr class="del-marker" data-after="${n}" title="Show in diff"><td class="ln"></td><td class="mk">−</td><td class="code">··· ${plural(c, 'deleted line')}</td></tr>`
      : '';
  };
  const rows = [marker(0)];
  html.forEach((code, i) => {
    const n = i + 1;
    rows.push(`<tr class="ln-row${added.has(n) ? ' add' : ''}" data-new="${n}"><td class="ln">${n}</td><td class="mk">${added.has(n) ? '+' : ''}</td><td class="code">${code}</td></tr>`);
    rows.push(marker(n));
  });
  if (!lines.length) rows.push('<tr class="note"><td colspan="3">Empty file.</td></tr>');
  return h('table', {
    class: 'code-table full',
    html: `<colgroup><col class="c-ln"><col class="c-mk"><col></colgroup><tbody>${rows.join('')}</tbody>`,
  });
}

// Flashes lines from..to, unfolding any #[cfg(test)] fold that hides them.
function flashRange(pane, side, from, to) {
  for (let l = from; l <= (to ?? from); l++) {
    const tr = pane.rows.get(anchorKey(side, l));
    if (!tr) continue;
    unfoldRow(pane, tr);
    flash(tr);
  }
}

// ───────────── #[cfg(test)] folds ─────────────
// Ranges come from GET /api/folds (language server or text scan, can be slow): fetched
// lazily once per Rust file and applied to already-rendered tables when they arrive.
// One bar per range in both views (the diff fold may start mid-hunk and span hunks).
// Default: collapsed while "Exclude tests" is on, else open; flipping the toggle resets
// every fold to that default and clears per-fold overrides (bar clicks, jumps), which
// are remembered per PR, keyed by range start.

const foldPrefKey = () => `review-master:folds:${prId()}`;

function loadFoldPrefs() {
  try {
    const saved = JSON.parse(localStorage.getItem(foldPrefKey()) || '{}');
    S.foldOverrides = new Map(saved && typeof saved === 'object' && !Array.isArray(saved)
      ? Object.entries(saved).filter(([, open]) => typeof open === 'boolean')
      : []);
  } catch {
    S.foldOverrides = new Map();
  }
}

function saveFoldPrefs() {
  try {
    localStorage.setItem(foldPrefKey(), JSON.stringify(Object.fromEntries(S.foldOverrides)));
  } catch {
    /* storage unavailable */
  }
}

// Sorted, valid ranges with nested ones dropped (the outermost range folds them).
function normalizeFolds(list) {
  const out = [];
  const valid = (Array.isArray(list) ? list : []).filter(r => Array.isArray(r) && r[0] > 0 && r[1] >= r[0]);
  for (const [s, e] of valid.sort((a, b) => a[0] - b[0] || b[1] - a[1])) {
    const last = out[out.length - 1];
    if (!last || s > last[1]) out.push([s, e]);
  }
  return out;
}

// One request per file per page load: the promise (including a failed, empty result)
// stays cached, so re-renders and viewer opens never refetch.
function fetchFolds(path) {
  if (!/\.rs$/i.test(path)) return Promise.resolve([]);
  let p = S.foldCache.get(path);
  if (!p) {
    p = api('GET', `/api/folds?path=${qpath(path)}`, undefined, true)
      .then(r => normalizeFolds(r && r.test_folds))
      .catch(() => []);
    S.foldCache.set(path, p);
  }
  return p;
}

async function attachFolds(pane) {
  const ranges = await fetchFolds(pane.path);
  if (!ranges.length || pane.folds.size) return;
  if (pane.kind === 'full') foldFullRows(pane, ranges);
  else foldDiffRows(pane, ranges);
  for (const f of pane.folds.values()) {
    const keepOpen = pane.lastReveal && pane.lastReveal.dataset.fold === f.key;
    const open = keepOpen || (S.foldOverrides.get(`${pane.path}|${f.key}`) ?? !S.scope.tests);
    if (!open) setFold(pane, f.key, false, false);
    else updateFoldBar(f);
  }
}

// Tests toggle flipped: every rendered fold goes to the new default, overrides are dropped.
function resetTestFolds() {
  S.foldOverrides.clear();
  saveFoldPrefs();
  const panes = [...S.fileCache.values()].map(fs => fs.pane);
  if (S.viewer) panes.push(S.viewer.pane);
  for (const pane of panes) {
    if (!pane) continue;
    for (const key of pane.folds.keys()) setFold(pane, key, !S.scope.tests, false);
  }
}

function foldFullRows(pane, ranges) {
  const markers = new Map();
  for (const tr of pane.table.querySelectorAll('tr.del-marker')) markers.set(+tr.dataset.after, tr);
  for (const [s, e] of ranges) {
    const first = pane.rows.get(`RIGHT:${s}`);
    if (!first) continue;
    const key = `full:${s}`;
    let n = 0;
    for (let l = s; l <= e; l++) {
      const tr = pane.rows.get(`RIGHT:${l}`);
      if (!tr) break;
      tr.dataset.fold = key;
      n++;
      if (l < e && markers.has(l)) markers.get(l).dataset.fold = key;
    }
    addFoldBar(pane, key, first, plural(n, 'line'));
  }
}

// Diff: per range, the rows whose new line lies inside it (deleted rows: between two
// in-range lines), widened over adjacent whitespace-only rows. The fold covers the
// contiguous span, so it can start mid-hunk and swallows hunk separators within it.
function foldDiffRows(pane, ranges) {
  const rows = [...pane.table.tBodies[0].rows].filter(tr => tr.classList.contains('ln-row') || tr.classList.contains('hunk'));
  const isLine = tr => tr.classList.contains('ln-row');
  const blank = tr => isLine(tr) && !tr.dataset.fold && !tr.cells[3].textContent.trim();
  for (const [s, e] of ranges) {
    const inside = tr => {
      if (!isLine(tr)) return false;
      if (tr.dataset.new) return +tr.dataset.new >= s && +tr.dataset.new <= e;
      const after = +tr.dataset.after;
      return s <= after && after < e;
    };
    let first = rows.findIndex(inside);
    if (first < 0) continue;
    let last = rows.findLastIndex(inside);
    while (first > 0 && blank(rows[first - 1])) first--;
    while (last < rows.length - 1 && blank(rows[last + 1])) last++;
    const key = `diff:${s}`;
    let lines = 0;
    let add = 0;
    let del = 0;
    for (let i = first; i <= last; i++) {
      const tr = rows[i];
      tr.dataset.fold = key;
      if (!isLine(tr)) continue;
      lines++;
      if (tr.classList.contains('add')) add++;
      else if (tr.classList.contains('del')) del++;
    }
    addFoldBar(pane, key, rows[first], `${plural(lines, 'line')}, +${add} −${del}`);
  }
}

function addFoldBar(pane, key, before, desc) {
  const label = h('span', { class: 'fold-label' });
  const bar = h('tr', { class: 'fold-bar', 'data-fold-bar': key, title: 'Fold or unfold this #[cfg(test)] code' },
    h('td', { colspan: String(pane.cols) }, label));
  before.before(bar);
  pane.folds.set(key, { key, bar, label, desc, open: true });
}

// Rows a fold hides: its tagged rows plus annotation rows (drafts, chips) anchored on them.
function foldRows(f) {
  const rows = [];
  for (let tr = f.bar.nextElementSibling; tr; tr = tr.nextElementSibling) {
    if (tr.dataset.fold !== f.key && !(rows.length && tr.classList.contains('annot'))) break;
    rows.push(tr);
  }
  return rows;
}

function setFold(pane, key, open, persist = true) {
  const f = pane.folds.get(key);
  if (!f) return;
  f.open = open;
  const rows = foldRows(f);
  for (const tr of rows) tr.hidden = !open;
  if (persist) {
    S.foldOverrides.set(`${pane.path}|${key}`, open);
    saveFoldPrefs();
  }
  updateFoldBar(f, rows);
}

// Closed bars also count the drafts/chats they hide; clicking the bar unfolds them.
function updateFoldBar(f, rows = null) {
  const hidden = f.open ? 0 : (rows || foldRows(f))
    .reduce((n, tr) => n + (tr.classList.contains('annot') ? tr.querySelectorAll('.card, .thread-chip, .composer').length : 0), 0);
  f.bar.classList.toggle('closed', !f.open);
  f.label.textContent = `${f.open ? '▾' : '▸'} #[cfg(test)] · ${f.desc}${hidden ? ` · ${plural(hidden, 'comment')}` : ''}`;
}

// After annotations re-render: new annotation rows inside closed folds stay hidden.
function refreshFolds(pane) {
  for (const f of pane.folds.values()) {
    if (f.open) continue;
    const rows = foldRows(f);
    for (const tr of rows) tr.hidden = true;
    updateFoldBar(f, rows);
  }
}

// Jump targets inside a closed fold open it first.
function unfoldRow(pane, tr) {
  pane.lastReveal = tr;
  const f = tr.dataset.fold && pane.folds.get(tr.dataset.fold);
  if (f && !f.open) setFold(pane, f.key, true);
}

// ───────────────────────────── file viewer ─────────────────────────────
// Overlay with a whole HEAD file: a changed file (added-line tints, deleted-line
// markers) or any other file of the checkout, e.g. one referenced by a chat answer.

async function openViewer(path, target = null) {
  const fs = S.files.get(path) || null;
  if (fs && fs.file.status === 'deleted') return;
  closeViewer();
  clearSel();
  const v = { path, fs, pane: null };
  S.viewer = v;
  const scroll = $('#viewer-scroll');
  v.lspChip = h('span', { class: 'chip lsp-chip', hidden: true });
  v.outlineBtn = h('button', { class: 'btn small outline-btn', hidden: true, title: 'Go to symbol', onclick: () => toggleOutline(v) }, 'Outline');
  $('#viewer-head').replaceChildren(
    fs ? statusBadge(fs.file.status) : null,
    fs ? fileTitle(fs.file) : pathLabel(path),
    fs ? counts(fs.file) : h('span', { class: 'chip' }, 'not changed in this PR'),
    h('span', { class: 'spacer' }),
    v.lspChip,
    v.outlineBtn,
    h('button', { class: 'btn small', onclick: closeViewer }, 'Close'));
  scroll.replaceChildren(h('div', { class: 'placeholder' }, 'Loading…'));
  $('#viewer').hidden = false;
  let src = S.sources.get(path);
  if (!src) {
    try {
      src = await get(`/api/source?path=${qpath(path)}`);
    } catch {
      if (S.viewer === v) closeViewer();
      return;
    }
    S.sources.set(path, src);
  }
  if (S.viewer !== v) return;
  v.pane = makePane(path, 'full', fullTable(path, src), makeNotes(), scroll);
  scroll.replaceChildren(v.pane.notes.root, v.pane.table);
  scroll.scrollTop = 0;
  renderAnnotations(v.pane);
  attachFolds(v.pane);
  watchViewerLsp(v);
  if (!target) return;
  const tr = v.pane.rows.get(`RIGHT:${target.line}`);
  if (!tr) {
    toast(`${path} has no line ${target.line}`, 'warn');
    return;
  }
  flashRange(v.pane, 'RIGHT', target.line, target.end);
  tr.scrollIntoView({ block: 'center' });
}

function closeViewer() {
  const v = S.viewer;
  if (!v) return;
  if (S.sel && S.sel.pane === v.pane) clearSel();
  closeSym();
  closeOutline();
  S.viewer = null;
  $('#viewer').hidden = true;
  $('#viewer-scroll').replaceChildren();
  // Composers opened in the viewer move back to their file's diff.
  let moved = false;
  for (const c of S.composers.values()) {
    if (c.inViewer) {
      c.inViewer = false;
      moved = true;
    }
  }
  if (moved && v.fs) renderAnnotations(v.fs.pane);
}

// Viewer "N deleted lines" marker → the file's inline diff, scrolled to those deletions.
async function jumpToDeleted(fs, after) {
  closeViewer();
  await ensureLoaded(fs);
  const p = fs.pane;
  const tr = p.table.querySelector(`tr.del[data-after="${after}"]`) ||
    [...p.rows.values()].find(r => r.dataset.new && +r.dataset.new > after);
  if (tr) unfoldRow(p, tr);
  (tr || fs.section).scrollIntoView({ block: tr ? 'center' : 'start' });
  if (tr) flash(tr);
}

// Pane and row where an anchor of a changed file is shown: the open viewer when it
// has the line, else the file's inline diff (closing the viewer).
async function anchorRow(fs, side, line) {
  const key = anchorKey(side, line);
  const v = S.viewer;
  if (v && v.fs === fs && v.pane && v.pane.rows.has(key)) return { pane: v.pane, tr: v.pane.rows.get(key) };
  closeViewer();
  await ensureLoaded(fs);
  return { pane: fs.pane, tr: fs.pane.rows.get(key) || null };
}

// Scrolls to and flashes an anchor (lines line..end). New-side lines outside the
// diff, and files that are not part of the change, open in the file viewer.
async function revealLine(path, side, line, end = null) {
  if (!(await ensureInScope(path))) return;
  const fs = S.files.get(path);
  if (!fs) return openViewer(path, { line, end });
  const { pane, tr } = await anchorRow(fs, side, line);
  if (tr) {
    flashRange(pane, side, line, end);
    tr.scrollIntoView({ block: 'center' });
  } else if ((side || 'RIGHT') === 'RIGHT' && fs.file.status !== 'deleted') {
    await openViewer(path, { line, end });
  } else {
    fs.section.scrollIntoView({ block: 'start' });
  }
}

// Click on a code reference in rendered markdown (chat answers, draft bodies).
function openCodeRef(rawPath, line, end) {
  let path = rawPath.replace(/^\.\//, '');
  const known = p => S.files.has(p) || S.excluded.has(p);
  if (!known(path) && /^[ab]\//.test(path) && known(path.slice(2))) path = path.slice(2); // diff prefixes
  closeViewer(); // changed files prefer their diff
  return revealLine(path, 'RIGHT', line, end != null && end > line ? end : null);
}

// ───────────────────────────── annotations (drafts, chat chips, composers) ─────────────────────────────

// Annotation box right below an anchor row; null when the row is not in the pane.
function boxAt(pane, akey) {
  let b = pane.boxes.get(akey);
  if (b) return b;
  const row = pane.rows.get(akey);
  if (!row) return null;
  b = makeBox();
  b.tr = h('tr', { class: 'annot' }, h('td', { colspan: String(pane.cols) }, b.root));
  row.after(b.tr);
  pane.boxes.set(akey, b);
  return b;
}

// Drafts and chat chips render in every pane showing their file; a composer lives
// in exactly one place (the viewer when opened there, else the inline diff).
function renderAnnotations(pane) {
  if (!pane) return;
  const inViewer = pane.kind === 'full';
  for (const b of [...pane.boxes.values(), pane.notes]) {
    b.drafts.replaceChildren();
    b.chips.replaceChildren();
  }
  const place = akey => boxAt(pane, akey) || pane.notes;
  for (const d of S.drafts) {
    if (d.path === pane.path && !S.composers.has(`d${d.id}`)) place(anchorKey(d.side, d.line)).drafts.append(draftCard(d));
  }
  for (const t of S.threads) {
    if (t.anchor && t.anchor.path === pane.path) place(anchorKey(t.anchor.side, t.anchor.line)).chips.append(threadChip(t));
  }
  for (const c of S.composers.values()) {
    if (c.path === pane.path && !!c.inViewer === inViewer) place(c.akey).composers.append(c.el);
  }
  for (const [k, b] of pane.boxes) {
    if (boxEmpty(b)) {
      b.tr.remove();
      pane.boxes.delete(k);
    }
  }
  pane.notes.root.hidden = boxEmpty(pane.notes);
  refreshFolds(pane);
}

function renderAllAnnotations() {
  for (const fs of S.order) renderAnnotations(fs.pane);
  if (S.viewer) renderAnnotations(S.viewer.pane);
}

function renderFileAnnotations(path) {
  const fs = S.files.get(path);
  if (fs) renderAnnotations(fs.pane);
  if (S.viewer && S.viewer.path === path) renderAnnotations(S.viewer.pane);
}

function draftCard(d) {
  return h('div', { class: 'card draft' },
    h('div', { class: 'card-head' },
      sevPill(d.severity),
      h('span', { class: 'card-title' }, 'Draft'),
      h('span', { class: 'dim' }, lineLabel(d)),
      h('span', { class: 'spacer' }),
      h('button', { class: 'link-btn', onclick: () => editDraft(d) }, 'Edit'),
      h('button', { class: 'link-btn danger', onclick: () => deleteDraft(d) }, 'Delete')),
    h('div', { class: 'md card-body', html: md(d.body) }),
    d.problem && h('div', { class: 'warn-line' }, `⚠ won't anchor: ${d.problem}; will go into the review body`));
}

function threadChip(t) {
  return h('button', { class: 'chip thread-chip', title: t.last, onclick: () => openThread(t.id) }, `💬 chat (${t.messages})`);
}

async function openComposer(a) {
  if (!(await ensureInScope(a.path))) return;
  const fs = S.files.get(a.path);
  if (!fs) {
    toast(`${a.path} is not among the changed files; comments can only go on changed files`, 'warn');
    return;
  }
  const key = a.draft ? `d${a.draft.id}` : `n:${a.side}:${a.start ?? ''}:${a.line}:${a.path}`;
  let c = S.composers.get(key);
  if (!c) {
    c = makeComposer(key, a);
    S.composers.set(key, c);
  } else if (a.body) {
    c.ta.value = a.body;
    c.ta.dispatchEvent(new Event('input'));
  }
  const { pane, tr } = await anchorRow(fs, a.side, a.line);
  if (tr) unfoldRow(pane, tr);
  c.inViewer = pane.kind === 'full';
  renderFileAnnotations(fs.path);
  c.el.scrollIntoView({ block: 'nearest' });
  c.ta.focus({ preventScroll: true });
}

function makeComposer(key, a) {
  const sev = h('select', { class: 'sev-select', 'aria-label': 'Severity' },
    SEVERITIES.map(s => h('option', { value: s }, s)));
  sev.value = a.severity || 'minor';
  const ta = h('textarea', { class: 'composer-text', rows: '3', placeholder: 'Leave a comment… (markdown, Ctrl+Enter to save)' });
  ta.value = a.body || '';
  autosize(ta);
  const save = h('button', { class: 'btn primary small' }, a.draft ? 'Update draft' : 'Save draft');
  const c = { key, path: a.path, akey: anchorKey(a.side, a.line), ta };
  const cancel = h('button', { class: 'btn small', onclick: () => closeComposer(c) }, 'Cancel');
  c.el = h('div', { class: 'composer' },
    h('div', { class: 'composer-head' },
      h('span', { class: 'composer-title' }, a.draft ? 'Edit draft' : 'New comment'),
      h('span', { class: 'dim' }, lineLabel(a)),
      h('span', { class: 'spacer' }), sev),
    ta,
    h('div', { class: 'composer-actions' }, cancel, save));
  const submit = async () => {
    const body = ta.value.trim();
    if (!body) return ta.focus();
    save.disabled = true;
    try {
      const r = await post('/api/draft', {
        id: a.draft ? a.draft.id : null, path: a.path, line: a.line, start: a.start ?? null,
        side: a.side, severity: sev.value, body,
      });
      if (r.problem) toast(`Draft saved, but it won't anchor: ${r.problem}. It will go into the review body.`, 'warn', 9000);
      closeComposer(c, false);
      await refreshDrafts();
    } finally {
      save.disabled = false;
    }
  };
  save.addEventListener('click', submit);
  onCtrlEnter(ta, submit);
  return c;
}

function closeComposer(c, rerender = true) {
  S.composers.delete(c.key);
  c.el.remove();
  if (rerender) renderFileAnnotations(c.path);
}

function editDraft(d) {
  openComposer({ path: d.path, side: d.side, line: d.line, start: d.start, draft: d, body: d.body, severity: d.severity });
}

async function deleteDraft(d) {
  if (!confirm(`Delete draft on ${d.path}:${lineLabel(d)}?`)) return;
  await api('DELETE', `/api/draft/${d.id}`);
  const c = S.composers.get(`d${d.id}`);
  if (c) closeComposer(c, false);
  await refreshDrafts();
}

async function refreshDrafts() {
  S.drafts = (await get('/api/drafts')) || [];
  renderAllAnnotations();
  renderDraftCounts();
  renderReviewDrafts();
}

async function refreshThreads() {
  S.threads = (await get('/api/threads')) || [];
  renderAllAnnotations();
  renderThreadList();
}

// ───────────────────────────── line selection & popover ─────────────────────────────

const rowAnchor = tr => (tr.dataset.new ? { side: 'RIGHT', line: +tr.dataset.new } : { side: 'LEFT', line: +tr.dataset.old });

function setSel(pane, side, from, to) {
  unmarkSel();
  const lo = Math.min(from, to);
  const hi = Math.max(from, to);
  const marked = [];
  for (let l = lo; l <= hi; l++) {
    const tr = pane.rows.get(`${side}:${l}`);
    if (tr) {
      tr.classList.add('sel');
      marked.push(tr);
    }
  }
  S.sel = { pane, side, from, start: lo === hi ? null : lo, line: hi, marked, snippet: null };
}

function unmarkSel() {
  if (S.sel) for (const tr of S.sel.marked) tr.classList.remove('sel');
}

function clearSel() {
  unmarkSel();
  S.sel = null;
  S.dragging = false;
  hidePopover();
}

// Gutter press starts (or with Shift extends) a line selection; anything else
// outside the popover drops the current selection.
function onDocMouseDown(e) {
  if (!(e.target instanceof Element)) return;
  if (S.sym && !e.target.closest('.sym-popup')) closeSym();
  if (S.scopePop && !e.target.closest('.scope-pop, .scope-link')) closeScopePop();
  if (S.viewer && S.viewer.outlineEl && !e.target.closest('.outline-pop, .outline-btn')) closeOutline();
  const codeCell = (e.metaKey || e.ctrlKey) && e.target.closest('tr.ln-row td.code');
  if (codeCell && paneOf(codeCell)) {
    e.preventDefault(); // Cmd/Ctrl-click navigates (see click handler) instead of selecting text
    if (S.sel) clearSel();
    return;
  }
  const td = e.target.closest('td.ln');
  const tr = td && td.closest('tr.ln-row');
  const pane = tr && paneOf(tr);
  if (pane && e.button === 0) {
    e.preventDefault(); // no text selection while picking lines
    window.getSelection().removeAllRanges();
    const a = rowAnchor(tr);
    const s = S.sel;
    if (e.shiftKey && s && s.pane === pane && s.side === a.side) setSel(pane, a.side, s.from, a.line);
    else setSel(pane, a.side, a.line, a.line);
    S.dragging = true;
    hidePopover();
  } else if (S.sel && !e.target.closest('.popover')) {
    clearSel();
  }
}

function onGutterOver(e) {
  if (!S.dragging || !S.sel) return;
  const tr = e.target.closest('tr.ln-row');
  if (!tr || !e.target.closest('td.ln') || paneOf(tr) !== S.sel.pane) return;
  const a = rowAnchor(tr);
  if (a.side === S.sel.side) setSel(S.sel.pane, a.side, S.sel.from, a.line);
}

function onDocMouseUp(e) {
  if (e.target instanceof Element && e.target.closest('.popover')) return;
  if (!S.dragging) {
    setTimeout(selectFromText); // the text selection settles after mouseup
    return;
  }
  S.dragging = false;
  const s = S.sel;
  const tr = s && s.pane.rows.get(`${s.side}:${s.line}`);
  if (!tr) return;
  const gutter = tr.cells[s.pane.kind === 'diff' ? 1 : 0].getBoundingClientRect();
  showPopover(tr.getBoundingClientRect().bottom, gutter.right + 8);
}

// Code text selected with the mouse → anchor on the rows it spans: RIGHT (new
// numbers) when any spanned row has one, else LEFT; single table only.
function selectFromText() {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || !sel.rangeCount) return;
  const range = sel.getRangeAt(0);
  const rowOf = node => {
    const el = node.nodeType === 1 ? node : node.parentElement;
    return el ? el.closest('tr') : null;
  };
  const first = rowOf(range.startContainer);
  const last = rowOf(range.endContainer);
  const pane = first && paneOf(first);
  if (!pane || !last || paneOf(last) !== pane) return;
  const body = pane.table.tBodies[0];
  if (first.parentNode !== body || last.parentNode !== body) return;
  if (first.classList.contains('annot') || last.classList.contains('annot')) return;
  let end = last.sectionRowIndex;
  if (end > first.sectionRowIndex && endsAtRowStart(range, last)) end--;
  const spanned = [];
  for (let k = first.sectionRowIndex; k <= end; k++) {
    if (body.rows[k].classList.contains('ln-row')) spanned.push(body.rows[k]);
  }
  const side = spanned.some(r => r.dataset.new) ? 'RIGHT' : 'LEFT';
  const nums = spanned.map(r => +(side === 'RIGHT' ? r.dataset.new : r.dataset.old)).filter(n => n > 0);
  if (!nums.length) return;
  const lo = nums.reduce((a, b) => Math.min(a, b));
  const hi = nums.reduce((a, b) => Math.max(a, b));
  unmarkSel();
  S.sel = { pane, side, from: lo, start: lo === hi ? null : lo, line: hi, marked: [], snippet: sel.toString().slice(0, 2000) };
  const rects = range.getClientRects();
  const r = rects.length ? rects[rects.length - 1] : range.getBoundingClientRect();
  showPopover(r.bottom, r.right);
}

// True when a range ends before any code text of `tr` (a triple-click ends at the next row's start).
function endsAtRowStart(range, tr) {
  const code = tr.querySelector('td.code');
  if (!code) return true;
  const r = document.createRange();
  r.setStart(code, 0);
  r.setEnd(range.endContainer, range.endOffset); // collapses when the end precedes the code cell
  return r.toString() === '';
}

function buildPopover() {
  const label = h('span', { class: 'pop-label' });
  const comment = h('button', { class: 'btn small primary', onclick: popComment }, 'Comment');
  // mousedown default would collapse a text selection before the click lands
  const el = h('div', { class: 'popover', hidden: true, onmousedown: e => e.preventDefault() },
    label, comment, h('button', { class: 'btn small', onclick: popAsk }, 'Ask'));
  S.pop = { el, label, comment };
}

// Places the popover at a viewport point, inside the selection pane's host so it scrolls along.
function showPopover(y, x) {
  const s = S.sel;
  if (!s) return;
  S.pop.label.textContent = lineLabel(s);
  S.pop.comment.hidden = !s.pane.fs; // drafts only anchor on changed files
  placeIn(S.pop.el, s.pane.host, x, y + 4, 240);
  S.pop.el.hidden = false;
}

// Absolutely positions `el` inside `host` at a viewport point, so it scrolls with the code.
function placeIn(el, host, x, y, width) {
  const hr = host.getBoundingClientRect();
  el.style.top = `${y - hr.top + host.scrollTop}px`;
  el.style.left = `${Math.max(0, Math.min(x - hr.left + host.scrollLeft, hr.width - width))}px`;
  host.append(el);
}

function hidePopover() {
  if (S.pop) S.pop.el.hidden = true;
}

// ───────────────────────────── code navigation (LSP) ─────────────────────────────

const LSP_BUSY = new Set(['starting', 'indexing']);
const IDENT_CHAR = /[\p{L}\p{N}_$]/u;
const IDENT = /^[\p{L}_$][\p{L}\p{N}_$]*$/u;
const lspStatusUrl = (path, start) => `/api/lsp/status?path=${qpath(path)}${start ? '&start=true' : ''}`;

// Starts the file's language server in the background, once per path.
function warmLsp(path) {
  if (S.lspWarm.has(path)) return;
  S.lspWarm.add(path);
  api('GET', lspStatusUrl(path, true), undefined, true).catch(() => {});
}

// Viewer header chip while the language server is starting/indexing, or failed.
async function watchViewerLsp(v) {
  S.lspWarm.add(v.path);
  for (let first = true; ; first = false) {
    let st;
    try {
      st = await api('GET', lspStatusUrl(v.path, first), undefined, true);
    } catch {
      return;
    }
    if (S.viewer !== v) return;
    const busy = LSP_BUSY.has(st.state);
    v.lspChip.hidden = !(busy || st.state === 'failed');
    v.outlineBtn.hidden = st.state === 'none' || st.state === 'failed';
    v.lspChip.className = `chip lsp-chip lsp-${st.state}`;
    v.lspChip.textContent = `${st.server || st.language || 'LSP'} ${st.state}${busy ? '…' : ''}`;
    v.lspChip.title = st.error || '';
    if (!busy) return;
    await sleep(2000);
  }
}

function caretAt(x, y) {
  if (document.caretPositionFromPoint) {
    const p = document.caretPositionFromPoint(x, y);
    return p && { node: p.offsetNode, offset: p.offset };
  }
  if (document.caretRangeFromPoint) {
    const r = document.caretRangeFromPoint(x, y);
    return r && { node: r.startContainer, offset: r.startOffset };
  }
  return null;
}

// Identifier-ish token under a viewport point in a code cell, with its 0-based
// UTF-16 start column in the line (JS string indices are UTF-16 code units).
function tokenAt(td, x, y) {
  const c = caretAt(x, y);
  if (!c || !td.contains(c.node)) return null;
  const r = document.createRange();
  r.setStart(td, 0);
  r.setEnd(c.node, c.offset);
  const text = td.textContent;
  const isW = ch => !!ch && IDENT_CHAR.test(ch);
  let i = r.toString().length;
  if (!isW(text[i]) && isW(text[i - 1])) i--;
  if (!isW(text[i])) return null;
  let s = i;
  let e = i + 1;
  while (s > 0 && isW(text[s - 1])) s--;
  while (e < text.length && isW(text[e])) e++;
  return { token: text.slice(s, e), character: s };
}

// ───────────── viewer outline ("go to symbol") ─────────────

// LSP SymbolKind numbers → short labels (servers may also send names).
const SYMBOL_KINDS = ['', 'file', 'module', 'namespace', 'package', 'class', 'method', 'property', 'field',
  'constructor', 'enum', 'interface', 'fn', 'var', 'const', 'string', 'number', 'bool', 'array', 'object',
  'key', 'null', 'variant', 'struct', 'event', 'operator', 'type param'];
const kindLabel = k => (typeof k === 'number' ? SYMBOL_KINDS[k] || 'symbol' : String(k || 'symbol').toLowerCase());

function closeOutline() {
  const v = S.viewer;
  if (!v || !v.outlineEl) return;
  v.outlineEl.remove();
  v.outlineEl = null;
}

function gotoSymbol(v, s) {
  closeOutline();
  const tr = v.pane && v.pane.rows.get(`RIGHT:${s.line}`);
  if (!tr) return;
  unfoldRow(v.pane, tr);
  tr.scrollIntoView({ block: 'center' });
  flash(tr);
}

// Filterable symbol list for the viewer's file; symbols are cached on the viewer.
async function toggleOutline(v) {
  if (v.outlineEl) return closeOutline();
  const input = h('input', { class: 'outline-filter', type: 'search', placeholder: 'Filter symbols…', 'aria-label': 'Filter symbols' });
  const list = h('ul', { class: 'outline-list' }, h('li', { class: 'hint' }, 'Loading outline…'));
  const el = h('div', { class: 'outline-pop' }, input, list);
  v.outlineEl = el;
  $('#viewer .viewer-box').append(el);
  input.focus();
  if (!v.outline) {
    try {
      const r = await api('GET', `/api/lsp/outline?path=${qpath(v.path)}`, undefined, true);
      v.outline = r.symbols || [];
    } catch (e) {
      if (v.outlineEl === el) list.replaceChildren(h('li', { class: 'hint' }, `outline unavailable: ${e.message}`));
      return;
    }
  }
  if (S.viewer !== v || v.outlineEl !== el) return;
  let shown = [];
  const render = () => {
    const q = input.value.trim().toLowerCase();
    shown = q ? v.outline.filter(s => s.name.toLowerCase().includes(q)) : v.outline;
    list.replaceChildren(...(shown.length
      ? shown.map(s => h('li', {}, h('button', { class: `outline-item d-${Math.min(s.depth || 0, 6)}`, title: `line ${s.line}`, onclick: () => gotoSymbol(v, s) },
        h('span', { class: 'tag' }, kindLabel(s.kind)),
        h('span', { class: 'outline-name' }, s.name),
        s.detail && h('span', { class: 'outline-detail' }, s.detail))))
      : [h('li', { class: 'hint' }, v.outline.length ? 'No matches.' : 'No symbols.')]));
  };
  input.addEventListener('input', render);
  input.addEventListener('keydown', e => {
    if (e.key === 'Enter' && shown.length) gotoSymbol(v, shown[0]);
  });
  render();
}

function closeSym() {
  if (!S.sym) return;
  S.sym.el.remove();
  S.sym = null;
}

// Clickable LSP/search location; `path: null` is outside the checkout and inert.
function locationEl(loc) {
  const label = loc.display || `${loc.path}:${loc.line}`;
  const kids = [h('span', { class: 'loc-label' }, label), loc.preview && h('code', { class: 'loc-preview' }, loc.preview.trim())];
  if (!loc.path) return h('li', { class: 'loc inert', title: 'Outside the checkout' }, kids);
  return h('li', {}, h('button', {
    class: 'loc', title: `${loc.path}:${loc.line}`,
    onclick: () => {
      closeSym();
      openCodeRef(loc.path, loc.line, null);
    },
  }, kids));
}

const locList = locs => h('ul', { class: 'loc-list' }, locs.map(locationEl));

// Cmd/Ctrl-click on code: hover + definitions (+ references on demand) from the
// language server; falls back to a whole-word text search for identifiers.
async function openSymbol(pane, tr, td, x, y) {
  const tok = tokenAt(td, x, y);
  if (!tok) return;
  closeSym();
  const line = tr.dataset.new ? +tr.dataset.new : null; // deleted lines are not in HEAD: search only
  const body = h('div', { class: 'sym-body' }, h('div', { class: 'working' }, h('span', { class: 'spinner' }), 'Looking up…'));
  const el = h('div', { class: 'sym-popup' },
    h('div', { class: 'sym-head' },
      h('code', {}, tok.token),
      line != null && h('span', { class: 'dim' }, `${pane.path}:${line}:${tok.character + 1}`),
      h('span', { class: 'spacer' }),
      h('button', { class: 'icon-btn', title: 'Close', 'aria-label': 'Close', onclick: closeSym }, '×')),
    body);
  const sym = { el };
  S.sym = sym;
  placeIn(el, pane.host, x, y + 14, 560);

  const pos = { path: pane.path, line, character: tok.character };
  const parts = [];
  let res = null;
  if (line != null) {
    try {
      res = await api('POST', '/api/lsp/symbol', pos, true);
    } catch (e) {
      parts.push(h('div', { class: 'hint' }, `Language server: ${e.message}`));
    }
  }
  if (S.sym !== sym) return;
  if (res && res.hover) parts.push(h('div', { class: 'md sym-hover', html: md(res.hover, { headings: false }) }));
  const defs = (res && res.definitions) || [];
  if (defs.length) parts.push(h('div', { class: 'sym-section' }, defs.length === 1 ? 'Definition' : `Definitions (${defs.length})`), locList(defs));
  if (res) {
    const refsBox = h('div', {});
    const refsBtn = h('button', {
      class: 'btn small',
      onclick: async () => {
        refsBtn.disabled = true;
        refsBtn.textContent = 'Finding references…';
        try {
          const r = await api('POST', '/api/lsp/references', pos);
          const refs = r.references || [];
          refsBox.replaceChildren(h('div', { class: 'sym-section' }, `References (${refs.length})`),
            refs.length ? locList(refs) : h('div', { class: 'hint' }, 'No references found.'));
        } catch {
          refsBtn.disabled = false;
          refsBtn.textContent = 'References';
        }
      },
    }, 'References');
    refsBox.append(refsBtn);
    parts.push(refsBox);
  }
  if (!defs.length && IDENT.test(tok.token)) {
    try {
      const r = await get(`/api/search?q=${encodeURIComponent(tok.token)}&word=true&limit=100`);
      if (S.sym !== sym) return;
      const hits = r.hits || [];
      parts.push(h('div', { class: 'sym-section' }, `Text matches (${hits.length}${r.truncated ? '+' : ''})`),
        hits.length ? locList(hits.map(hit => ({ ...hit, display: `${hit.path}:${hit.line}` }))) : h('div', { class: 'hint' }, 'No matches.'));
    } catch {
      /* toasted */
    }
  }
  if (S.sym !== sym) return;
  if (!parts.length) parts.push(h('div', { class: 'hint' }, 'No symbol information.'));
  body.replaceChildren(...parts);
}

function popComment() {
  const s = S.sel;
  if (!s) return;
  clearSel();
  window.getSelection().removeAllRanges();
  openComposer({ path: s.pane.path, side: s.side, line: s.line, start: s.start });
}

function popAsk() {
  const s = S.sel;
  if (!s) return;
  clearSel();
  window.getSelection().removeAllRanges();
  openChatNew({ path: s.pane.path, side: s.side, line: s.line, start: s.start }, s.snippet);
}

// ───────────────────────────── drawers ─────────────────────────────

function openDrawer(name) {
  S.drawer = name;
  $('#chat-drawer').hidden = name !== 'chat';
  $('#review-drawer').hidden = name !== 'review';
  document.body.classList.toggle('drawer-open', !!name);
  $('#btn-chat').classList.toggle('on', name === 'chat');
  $('#btn-review').classList.toggle('on', name === 'review');
  if (name === 'chat') {
    renderThreadList();
    renderConvo();
  } else if (name === 'review') renderReview();
}

const toggleDrawer = name => S.info && openDrawer(S.drawer === name ? null : name);

// ───────────────────────────── chat ─────────────────────────────

function makeActivity() {
  const sum = h('summary');
  const list = h('div', { class: 'activity-lines' });
  return { lines: [], running: true, shown: 0, sum, list, el: h('details', { class: 'activity' }, sum, list) };
}

// Appends new progress lines to an activity log, keeping it scrolled to the bottom.
function updateActivity(a) {
  const stick = a.list.scrollTop + a.list.clientHeight >= a.list.scrollHeight - 4;
  for (; a.shown < a.lines.length; a.shown++) a.list.append(h('div', { class: 'activity-line' }, a.lines[a.shown]));
  if (stick) a.list.scrollTop = a.list.scrollHeight;
  a.el.classList.toggle('running', a.running);
  a.sum.textContent = `${a.running ? 'Working…' : 'Activity'} · ${plural(a.lines.length, 'step')}`;
  return a.el;
}

function renderThreadList() {
  if (S.drawer !== 'chat') return;
  const list = $('#thread-list');
  if (!S.threads.length) {
    list.replaceChildren(h('div', { class: 'empty' }, 'No conversations yet. Select code lines and click Ask, or ask about the whole PR below.'));
    return;
  }
  list.replaceChildren(...S.threads.map(t =>
    h('button', { class: `thread-item${t.id === S.chat.threadId ? ' on' : ''}`, onclick: () => openThread(t.id) },
      h('span', { class: 'thread-anchor' }, anchorText(t.anchor)),
      h('span', { class: 'count' }, t.messages),
      h('span', { class: 'thread-last' }, t.last))));
}

// New thread composer; `snippet` is selected code text sent along with the first question.
function openChatNew(anchor, snippet = null) {
  Object.assign(S.chat, { threadId: null, anchor, snippet, thread: null });
  openDrawer('chat');
  $('#chat-input').focus();
}

async function openThread(id) {
  Object.assign(S.chat, { threadId: id, anchor: null, snippet: null });
  if (!S.chat.thread || S.chat.thread.id !== id) S.chat.thread = null;
  openDrawer('chat');
  const t = await get(`/api/thread?id=${id}`);
  if (S.chat.threadId !== id) return;
  S.chat.thread = t;
  renderConvo();
  const job = S.chat.jobs.get(id);
  if (t.busy && !(job && job.act.running)) watchBusy(id);
}

// A thread can be busy from a job this page did not start (e.g. after reload): poll it.
async function watchBusy(id) {
  while (S.chat.threadId === id && S.drawer === 'chat') {
    await sleep(1500);
    if (S.chat.threadId !== id) return;
    const t = await get(`/api/thread?id=${id}`);
    if (S.chat.threadId !== id) return;
    S.chat.thread = t;
    renderConvo();
    if (!t.busy) break;
  }
  await refreshThreads();
}

async function reloadThread(id) {
  const t = await get(`/api/thread?id=${id}`);
  if (S.chat.threadId === id) {
    S.chat.thread = t;
    renderConvo();
  }
}

const msgEl = m => (m.role === 'assistant'
  ? h('div', { class: 'msg assistant md', html: md(m.text) })
  : h('div', { class: 'msg user' }, m.text));

function renderConvo() {
  if (S.drawer !== 'chat') return;
  const c = S.chat;
  const t = c.thread && c.thread.id === c.threadId ? c.thread : null;
  const anchor = c.threadId != null ? (t ? t.anchor : null) : c.anchor;
  const job = c.threadId != null ? c.jobs.get(c.threadId) : null;

  const msgs = t ? [...t.messages] : [];
  const lastAnswer = [...msgs].reverse().find(m => m.role === 'assistant');
  const chip = anchor
    ? h('button', {
      class: 'anchor-chip', title: 'Jump to code',
      onclick: () => revealLine(anchor.path, anchor.side || 'RIGHT', anchor.start ?? anchor.line, anchor.start != null ? anchor.line : null),
    }, anchorText(anchor))
    : h('span', { class: 'anchor-chip whole' }, 'Whole PR');
  const draftBtn = c.threadId != null && h('button', {
    class: 'btn small', disabled: !(anchor && lastAnswer),
    title: anchor ? 'Prefill a comment at the anchor with the last answer' : 'This thread has no code anchor',
    onclick: () => openComposer({ path: anchor.path, side: anchor.side || 'RIGHT', line: anchor.line, start: anchor.start ?? null, body: lastAnswer.text }),
  }, 'Draft comment from this');
  $('#convo-head').replaceChildren(chip, h('span', { class: 'spacer' }), draftBtn);

  // Show the pending question until the thread itself reflects it, and the job's
  // answer if the stored thread lags behind.
  const running = job && job.act.running;
  const last = msgs[msgs.length - 1];
  if (running && !(last && last.role === 'user' && last.text === job.question)) msgs.push({ role: 'user', text: job.question });
  if (job && !running && job.answer && (!last || last.role !== 'assistant')) msgs.push({ role: 'assistant', text: job.answer });

  const nodes = msgs.map(msgEl);
  if (job && (running || job.act.lines.length)) {
    const at = !running && nodes.length && msgs[msgs.length - 1].role === 'assistant' ? nodes.length - 1 : nodes.length;
    nodes.splice(at, 0, updateActivity(job.act));
  } else if (t && t.busy) {
    nodes.push(h('div', { class: 'working' }, h('span', { class: 'spinner' }), 'Working…'));
  }
  if (job && job.error) nodes.push(h('div', { class: 'msg error' }, job.error));
  if (!nodes.length) {
    nodes.push(h('div', { class: 'empty' }, anchor ? `Ask a question about ${anchorText(anchor)}.` : 'Ask a question about the whole pull request.'));
  }
  if (c.threadId == null && c.snippet) {
    nodes.unshift(h('div', { class: 'snippet' }, h('div', { class: 'snippet-label' }, 'Selected code'), h('pre', {}, c.snippet)));
  }
  const box = $('#messages');
  box.replaceChildren(...nodes);
  box.scrollTop = box.scrollHeight;
  $('#chat-send').disabled = !!(running || (t && t.busy));
}

async function sendChat() {
  const input = $('#chat-input');
  const question = input.value.trim();
  if (!question || $('#chat-send').disabled) return;
  const from = S.chat.threadId;
  const a = S.chat.anchor;
  const snippet = from == null ? S.chat.snippet : null;
  const body = from != null
    ? { thread: from, path: null, start: null, line: null, side: null, snippet: null, question }
    : { thread: null, path: a ? a.path : null, start: a ? a.start ?? null : null, line: a ? a.line : null, side: a ? a.side : null, snippet, question };
  $('#chat-send').disabled = true;
  let r;
  try {
    r = await post('/api/chat', body);
  } catch {
    $('#chat-send').disabled = false;
    return;
  }
  input.value = '';
  input.dispatchEvent(new Event('input'));
  const job = { question, act: makeActivity(), error: null, answer: null };
  S.chat.jobs.set(r.thread, job);
  const viewing = () => S.drawer === 'chat' && S.chat.threadId === r.thread;
  if (S.chat.threadId === from) {
    // still looking at the same conversation: it is now (or remains) r.thread
    S.chat.threadId = r.thread;
    S.chat.anchor = null;
    S.chat.snippet = null;
    if (!S.chat.thread || S.chat.thread.id !== r.thread) S.chat.thread = { id: r.thread, anchor: a || null, messages: [], busy: true };
  }
  renderConvo();
  refreshThreads().catch(() => {});
  try {
    const result = await pollJob(r.job, lines => {
      job.act.lines.push(...lines);
      if (viewing()) updateActivity(job.act);
    });
    job.answer = result.answer || null;
  } catch (e) {
    job.error = e.message;
    if (!e.toasted) toast(`Chat failed: ${e.message}`, 'error');
  }
  job.act.running = false;
  updateActivity(job.act);
  if (viewing()) await reloadThread(r.thread).catch(() => renderConvo());
  await refreshThreads();
}

// ───────────────────────────── review drawer ─────────────────────────────

function renderReview() {
  const info = S.info;
  const rv = S.review;
  const local = info.mode === 'local';
  const own = !!(info.pr && info.viewer && info.viewer === info.pr.author);
  const locked = !!info.submitted;
  const restricted = local || own;
  if (restricted && rv.event !== 'comment') rv.event = 'comment';

  const summary = h('textarea', { class: 'review-summary', rows: '6', disabled: locked, placeholder: 'Review summary (markdown, optional)…' });
  summary.value = rv.body;
  summary.addEventListener('input', () => { rv.body = summary.value; });
  autosize(summary);

  const radios = EVENTS.map(ev => {
    const off = locked || (ev.value !== 'comment' && restricted);
    return h('label', { class: `radio${off ? ' disabled' : ''}` },
      h('input', { type: 'radio', name: 'review-event', value: ev.value, checked: rv.event === ev.value, disabled: off, onchange: () => { rv.event = ev.value; } }),
      h('span', { class: 'radio-text' }, h('strong', {}, ev.label), h('span', { class: 'dim' }, ev.hint)));
  });

  rv.list = h('div', { class: 'review-drafts' });
  const submit = h('button', { class: 'btn primary', disabled: local || locked || rv.submitting, onclick: submitReview },
    rv.submitting ? 'Submitting…' : 'Submit review');

  $('#review-body').replaceChildren(...[
    locked && submittedNode(),
    local && h('div', { class: 'banner info' }, 'Local mode: this worktree has no known pull request, so submitting is disabled. Chat and drafts still work.'),
    h('label', { class: 'field-label' }, 'Summary'),
    summary,
    h('div', { class: 'field-label' }, 'Event'),
    h('div', { class: 'radios' }, radios),
    own && !local && h('div', { class: 'hint' }, 'You authored this pull request; GitHub only allows comment reviews on your own PR.'),
    rv.list,
    h('div', { class: 'review-actions' }, submit),
  ].filter(Boolean));
  renderReviewDrafts();
}

function renderReviewDrafts() {
  const list = S.review.list;
  if (!list || S.drawer !== 'review') return;
  const order = d => (S.files.get(d.path) ? S.files.get(d.path).idx : 1e9);
  const drafts = [...S.drafts].sort((a, b) => order(a) - order(b) || a.path.localeCompare(b.path) || a.line - b.line);
  const items = drafts.map(d => {
    const first = (d.body.split('\n').find(l => l.trim()) || '').trim();
    return h('li', { class: 'review-draft' },
      h('div', { class: 'rd-head' },
        sevPill(d.severity),
        S.excluded.has(d.path) && h('span', { class: 'tag tag-excluded' }, "excluded — won't be submitted"),
        h('button', { class: 'link-btn rd-loc', title: 'Jump to line', onclick: () => revealLine(d.path, d.side, d.line) }, `${d.path}:${lineLabel(d)}`),
        h('span', { class: 'spacer' }),
        h('button', { class: 'icon-btn', title: 'Delete draft', 'aria-label': 'Delete draft', onclick: () => deleteDraft(d) }, '×')),
      h('div', { class: 'rd-first' }, first),
      d.problem && h('div', { class: 'warn-line' }, `⚠ won't anchor: ${d.problem}; will go into the review body`));
  });
  list.replaceChildren(
    h('div', { class: 'field-label' }, `Draft comments (${drafts.length})`),
    drafts.length ? h('ul', { class: 'rd-list' }, items) : h('div', { class: 'empty' }, 'No drafts yet. Select lines in a diff and click Comment.'));
}

async function submitReview() {
  const ev = EVENTS.find(e => e.value === S.review.event);
  const n = S.drafts.filter(d => !S.excluded.has(d.path)).length;
  if (!confirm(`Submit review as "${ev.label}" with ${plural(n, 'draft comment')}?`)) return;
  S.review.submitting = true;
  renderReview();
  try {
    const r = await post('/api/submit', { event: S.review.event, body: S.review.body });
    S.info.submitted = r.url;
    S.review.body = '';
    renderBanner();
    toast('Review submitted', 'ok');
    refreshDrafts().catch(() => {});
  } finally {
    S.review.submitting = false;
    if (S.drawer === 'review') renderReview();
  }
}

// ───────────────────────────── PR summary card ─────────────────────────────
// Generated in the background by the server; never blocks browsing.

const SUMMARY_COLLAPSED_KEY = 'review-master:summary-collapsed';

async function loadSummary() {
  const st = S.summary;
  let r;
  try {
    r = await api('GET', '/api/summary', undefined, true);
  } catch (e) {
    Object.assign(st, { error: e.message, loadFailed: true });
    renderSummary();
    return;
  }
  Object.assign(st, { text: r.summary || null, error: null, loadFailed: false });
  renderSummary();
  if (r.job != null) await runSummaryJob(r.job);
}

async function runSummaryJob(job) {
  const st = S.summary;
  Object.assign(st, { act: makeActivity(), running: true, error: null });
  renderSummary();
  try {
    const res = await pollJob(job, lines => {
      st.act.lines.push(...lines);
      updateActivity(st.act);
    }, true);
    st.text = res.summary || st.text;
  } catch (e) {
    st.error = e.message;
  }
  st.running = false;
  st.act.running = false;
  renderSummary();
}

async function regenerateSummary() {
  const st = S.summary;
  if (st.running) return;
  let r;
  try {
    r = await api('POST', '/api/summary', undefined, true);
  } catch (e) {
    st.error = e.message;
    renderSummary();
    return;
  }
  await runSummaryJob(r.job);
}

function toggleSummary() {
  const st = S.summary;
  st.collapsed = !st.collapsed;
  try {
    localStorage.setItem(SUMMARY_COLLAPSED_KEY, st.collapsed ? '1' : '0');
  } catch {
    /* storage unavailable */
  }
  renderSummary();
}

function renderSummary() {
  const st = S.summary;
  const card = $('#summary-card');
  card.hidden = false;
  card.classList.toggle('collapsed', st.collapsed);
  const head = h('div', { class: 'summary-head' },
    h('button', { class: 'summary-toggle', 'aria-expanded': String(!st.collapsed), onclick: toggleSummary },
      h('span', { class: 'chevron' }, '▾'), h('strong', {}, 'Summary')),
    st.running && h('span', { class: 'dim summary-status' }, h('span', { class: 'spinner' }), ' generating…'),
    h('span', { class: 'spacer' }),
    h('button', { class: 'icon-btn', title: 'Regenerate', 'aria-label': 'Regenerate summary', disabled: st.running, onclick: regenerateSummary }, '↻'));
  const body = h('div', { class: 'summary-body' });
  if (st.error) {
    body.append(h('div', { class: 'msg error' }, `Summary failed: ${st.error} `,
      h('button', { class: 'btn small', onclick: () => (st.loadFailed ? loadSummary() : regenerateSummary()) }, 'Retry')));
  }
  if (st.running) {
    body.append(h('div', { class: 'hint' }, 'The summary is being generated — browsing works meanwhile.'), updateActivity(st.act));
  }
  if (st.text) body.append(h('div', { class: 'md summary-text', html: md(st.text) }));
  else if (!st.running && !st.error) {
    body.append(h('div', { class: 'hint' }, 'No summary yet. ', h('button', { class: 'btn small', onclick: regenerateSummary }, 'Generate')));
  }
  card.replaceChildren(head, body);
}

// ───────────────────────────── wiring & boot ─────────────────────────────

function wire() {
  document.addEventListener('mousedown', onDocMouseDown);
  document.addEventListener('mouseover', onGutterOver);
  document.addEventListener('mouseup', onDocMouseUp);
  document.addEventListener('selectionchange', () => {
    // clearing a text selection dismisses its popover
    if (S.sel && S.sel.snippet != null && window.getSelection().isCollapsed) clearSel();
  });
  document.addEventListener('click', e => {
    if (!(e.target instanceof Element)) return;
    const codeCell = (e.metaKey || e.ctrlKey) && e.target.closest('tr.ln-row td.code');
    const codePane = codeCell && paneOf(codeCell);
    if (codePane) {
      e.preventDefault();
      openSymbol(codePane, codeCell.parentElement, codeCell, e.clientX, e.clientY);
      return;
    }
    const ref = e.target.closest('a.code-ref');
    if (ref) {
      e.preventDefault();
      openCodeRef(ref.dataset.path, +ref.dataset.line, ref.dataset.end ? +ref.dataset.end : null);
      return;
    }
    const bar = e.target.closest('tr.fold-bar');
    const barPane = bar && paneOf(bar);
    if (barPane) {
      const f = barPane.folds.get(bar.dataset.foldBar);
      if (f) setFold(barPane, f.key, !f.open);
      return;
    }
    const marker = e.target.closest('tr.del-marker');
    const pane = marker && paneOf(marker);
    if (pane && pane.fs) jumpToDeleted(pane.fs, +marker.dataset.after);
  });
  $('#viewer').addEventListener('click', e => {
    if (e.target.id === 'viewer') closeViewer(); // backdrop
  });
  document.addEventListener('keydown', e => {
    if (e.key !== 'Escape') return;
    if (S.sym) closeSym();
    else if (S.scopePop) closeScopePop();
    else if (S.viewer && S.viewer.outlineEl) closeOutline();
    else if (S.viewer) closeViewer();
    else if (S.sel) clearSel();
    else if (S.drawer) openDrawer(null);
  });

  let ticking = false;
  $('#main').addEventListener('scroll', () => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(() => {
      ticking = false;
      trackCurrent();
    });
  }, { passive: true });

  $('#btn-chat').addEventListener('click', () => toggleDrawer('chat'));
  $('#btn-review').addEventListener('click', () => toggleDrawer('review'));
  for (const b of document.querySelectorAll('[data-close]')) b.addEventListener('click', () => openDrawer(null));
  $('#chat-new').addEventListener('click', () => openChatNew(null));
  $('#chat-send').addEventListener('click', sendChat);
  const input = $('#chat-input');
  onCtrlEnter(input, sendChat);
  autosize(input);

  window.addEventListener('unhandledrejection', e => {
    const err = e.reason;
    if (err && err.toasted) e.preventDefault();
    else toast(String((err && err.message) || err), 'error');
  });
}

async function boot() {
  wire();
  buildPopover();
  try {
    S.info = await get('/api/info');
  } catch (e) {
    $('#pr-title').textContent = 'Failed to load review';
    $('#sections').replaceChildren(h('div', { class: 'empty big' }, `Could not load /api/info: ${e.message}`));
    return;
  }
  renderHeader();
  renderBanner();
  S.excluded = new Set(S.info.excluded || []);
  for (const f of S.info.files) S.allPaths.add(f.path);
  for (const p of S.excluded) S.allPaths.add(p);
  reconcileScope();
  S.io = new IntersectionObserver(entries => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      S.io.unobserve(en.target);
      const fs = fileOf(en.target);
      if (fs) ensureLoaded(fs).catch(() => {});
    }
  }, { root: $('#main'), rootMargin: '600px 0px' });
  $('#hide-tests').addEventListener('change', e => setExcludeTests(e.target.checked));
  loadFoldPrefs();
  renderScope(S.info.files.filter(f => !S.excluded.has(f.path)));
  try {
    S.summary.collapsed = localStorage.getItem(SUMMARY_COLLAPSED_KEY) === '1';
  } catch {
    /* storage unavailable */
  }
  loadSummary(); // runs in the background
  const [drafts, threads] = await Promise.all([get('/api/drafts'), get('/api/threads')]);
  S.drafts = drafts || [];
  S.threads = threads || [];
  renderDraftCounts();
  renderAllAnnotations();
}

boot();
