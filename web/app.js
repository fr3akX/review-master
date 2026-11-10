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
  sel: null, // { fs, side, from, start, line, view, marked }
  dragging: false,
  pop: null, // { el, label }
  composers: new Map(), // key → { key, path, akey, el, ta }
  drawer: null, // 'chat' | 'review' | null
  chat: { threadId: null, anchor: null, thread: null, jobs: new Map() }, // jobs: threadId → ChatJob
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
async function api(method, url, body) {
  const init = { method, headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(url, init);
  } catch (e) {
    throw failed(`Network error (${method} ${url}): ${e.message}`);
  }
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (!res.ok) throw failed((data && data.error) || `${method} ${url} failed: HTTP ${res.status}`);
  return data;
}
function failed(msg) {
  toast(msg, 'error');
  const e = new Error(msg);
  e.toasted = true;
  return e;
}
const get = url => api('GET', url);
const post = (url, body) => api('POST', url, body);
const qpath = path => encodeURIComponent(path);

// Shared job poller for chat turns: streams progress lines, resolves with result.
async function pollJob(id, onProgress) {
  let since = 0;
  for (;;) {
    const j = await get(`/api/job/${id}?since=${since}`);
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

function md(src) {
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
      out.push(`<h${lvl}>${inline(m[2])}</h${lvl}>`);
      i++;
    } else if (/^\s*>/.test(line)) {
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*> ?/, ''));
      out.push(`<blockquote>${md(quote.join('\n'))}</blockquote>`);
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

function inline(text) {
  const slots = [];
  const keep = html => `\u0000${slots.push(html) - 1}\u0000`;
  let s = String(text).replace(/`([^`]+)`/g, (_, code) => keep(`<code>${esc(code)}</code>`));
  s = esc(s)
    .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, (_, t, url) =>
      keep(`<a href="${url}" target="_blank" rel="noopener noreferrer">${t}</a>`))
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

function renderDraftCounts() {
  const n = S.drafts.length;
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

function buildSidebar() {
  const files = S.info.files;
  const add = files.reduce((n, f) => n + f.additions, 0);
  const del = files.reduce((n, f) => n + f.deletions, 0);
  $('#file-summary').replaceChildren(plural(files.length, 'file'), ' ', counts({ additions: add, deletions: del }));
  const list = $('#file-list');
  for (const fs of S.order) {
    fs.navBadge = h('span', { class: 'nav-drafts', title: 'drafts', hidden: true });
    fs.nav = h('li', {},
      h('button', { class: 'file-item', title: fs.path, onclick: () => jumpToFile(fs) },
        statusBadge(fs.file.status), pathLabel(fs.path), fs.navBadge, counts(fs.file)));
    list.append(fs.nav);
  }
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
  let cur = S.order[0];
  for (const fs of S.order) {
    if (fs.section.getBoundingClientRect().top <= top) cur = fs;
    else break;
  }
  setCurrent(cur);
}

function jumpToFile(fs) {
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

function buildSections() {
  const wrap = $('#sections');
  if (!S.info.files.length) {
    wrap.replaceChildren(h('div', { class: 'empty big' }, 'No changed files.'));
    return;
  }
  S.info.files.forEach((f, idx) => {
    const fs = {
      file: f, path: f.path, idx, view: 'diff', rendered: null, loading: null,
      diff: null, source: null, rows: new Map(), boxes: new Map(), cols: 4,
    };
    const deleted = f.status === 'deleted';
    fs.btnDiff = h('button', { class: 'seg on', onclick: () => setView(fs, 'diff') }, 'Diff');
    fs.btnFull = h('button', {
      class: 'seg', disabled: deleted, title: deleted ? 'File was deleted; only the diff is available' : 'Whole file at HEAD',
      onclick: () => setView(fs, 'full'),
    }, 'Full');
    const name = f.status === 'renamed' && f.old_path
      ? h('span', { class: 'rename' }, pathLabel(f.old_path), h('span', { class: 'arrow' }, ' → '), pathLabel(f.path))
      : pathLabel(f.path);
    fs.head = h('div', { class: 'file-head' },
      statusBadge(f.status), name, counts(f), h('span', { class: 'spacer' }),
      h('div', { class: 'seg-group' }, fs.btnDiff, fs.btnFull));
    fs.notes = makeBox();
    fs.notes.root.classList.add('file-notes');
    fs.notes.root.prepend(h('div', { class: 'notes-title' }, 'Comments on lines not shown in this view'));
    fs.notes.root.hidden = true;
    fs.body = h('div', { class: 'file-body' }, h('div', { class: 'placeholder' }, 'Loading…'));
    fs.section = h('section', { class: 'file', 'data-idx': idx }, fs.head, fs.notes.root, fs.body);
    S.files.set(f.path, fs);
    S.order.push(fs);
    wrap.append(fs.section);
  });

  const io = new IntersectionObserver(entries => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      io.unobserve(en.target);
      ensureLoaded(fileOf(en.target)).catch(() => {});
    }
  }, { root: $('#main'), rootMargin: '600px 0px' });
  for (const fs of S.order) io.observe(fs.section);
}

const fileOf = el => {
  const sec = el.closest('section.file');
  return sec ? S.order[+sec.dataset.idx] : null;
};

function syncToggle(fs) {
  fs.btnDiff.classList.toggle('on', fs.view === 'diff');
  fs.btnFull.classList.toggle('on', fs.view === 'full');
}

async function setView(fs, view) {
  if (view === 'full' && fs.file.status === 'deleted') return;
  fs.view = view;
  syncToggle(fs);
  await ensureLoaded(fs);
}

// Loads and renders whichever view is current; loops if the view changed mid-load.
async function ensureLoaded(fs) {
  while (fs.rendered !== fs.view) {
    if (!fs.loading) fs.loading = loadView(fs, fs.view).finally(() => { fs.loading = null; });
    await fs.loading;
  }
}

async function loadView(fs, view) {
  fs.body.classList.add('loading');
  try {
    if (view === 'diff') fs.diff ??= await get(`/api/diff?path=${qpath(fs.path)}`);
    else fs.source ??= await get(`/api/source?path=${qpath(fs.path)}`);
  } catch (e) {
    if (view === 'full') {
      fs.view = 'diff'; // fall back to the diff; ensureLoaded continues with it
      syncToggle(fs);
      return;
    }
    fs.body.replaceChildren(h('div', { class: 'load-error' }, `Could not load diff: ${e.message} `,
      h('button', { class: 'btn small', onclick: () => ensureLoaded(fs).catch(() => {}) }, 'Retry')));
    throw e;
  } finally {
    fs.body.classList.remove('loading');
  }
  if (fs.view === view) renderView(fs, view);
}

function renderView(fs, view) {
  if (S.sel && S.sel.fs === fs) clearSel();
  const table = view === 'diff' ? diffTable(fs) : fullTable(fs);
  fs.rows = new Map();
  fs.boxes = new Map();
  fs.cols = view === 'diff' ? 4 : 3;
  for (const tr of table.tBodies[0].rows) {
    const d = tr.dataset;
    if (d.old) fs.rows.set(`LEFT:${d.old}`, tr);
    if (d.new) fs.rows.set(`RIGHT:${d.new}`, tr);
  }
  fs.body.replaceChildren(table);
  fs.rendered = view;
  renderAnnotations(fs);
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

function fullTable(fs) {
  const src = fs.source;
  let text = src.text || '';
  if (text.endsWith('\n')) text = text.slice(0, -1);
  const lines = text ? text.split('\n') : [];
  const lang = lines.length <= HL_LINE_LIMIT ? hlLang(src.lang) || langForPath(fs.path) : null;
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

// Full-view "N deleted lines" marker → switch to the diff, scrolled to those deletions.
async function jumpToDeleted(fs, after) {
  await setView(fs, 'diff');
  const tr = fs.body.querySelector(`tr.del[data-after="${after}"]`) ||
    [...fs.rows.values()].find(r => r.dataset.new && +r.dataset.new > after);
  if (!tr) return;
  tr.scrollIntoView({ block: 'center' });
  flash(tr);
}

// Ensures the file is rendered and returns the row for an anchor, switching a
// Full view to Diff when the anchor (e.g. a LEFT line) only exists there.
async function anchorRow(fs, side, line) {
  await ensureLoaded(fs);
  const key = anchorKey(side, line);
  if (!fs.rows.has(key) && fs.rendered === 'full') await setView(fs, 'diff');
  return fs.rows.get(key) || null;
}

async function revealLine(path, side, line) {
  const fs = S.files.get(path);
  if (!fs) {
    toast(`${path} is not among the changed files`, 'warn');
    return;
  }
  const tr = await anchorRow(fs, side, line);
  (tr || fs.section).scrollIntoView({ block: tr ? 'center' : 'start' });
  if (tr) flash(tr);
}

// ───────────────────────────── annotations (drafts, chat chips, composers) ─────────────────────────────

// Annotation box right below an anchor row; null when the row is not in the current view.
function boxAt(fs, akey) {
  let b = fs.boxes.get(akey);
  if (b) return b;
  const row = fs.rows.get(akey);
  if (!row) return null;
  b = makeBox();
  b.tr = h('tr', { class: 'annot' }, h('td', { colspan: String(fs.cols) }, b.root));
  row.after(b.tr);
  fs.boxes.set(akey, b);
  return b;
}

function renderAnnotations(fs) {
  if (!fs.rendered) return;
  for (const b of [...fs.boxes.values(), fs.notes]) {
    b.drafts.replaceChildren();
    b.chips.replaceChildren();
  }
  const place = akey => boxAt(fs, akey) || fs.notes;
  for (const d of S.drafts) {
    if (d.path === fs.path && !S.composers.has(`d${d.id}`)) place(anchorKey(d.side, d.line)).drafts.append(draftCard(d));
  }
  for (const t of S.threads) {
    if (t.anchor && t.anchor.path === fs.path) place(anchorKey(t.anchor.side, t.anchor.line)).chips.append(threadChip(t));
  }
  for (const c of S.composers.values()) if (c.path === fs.path) place(c.akey).composers.append(c.el);
  for (const [k, b] of fs.boxes) {
    if (boxEmpty(b)) {
      b.tr.remove();
      fs.boxes.delete(k);
    }
  }
  fs.notes.root.hidden = boxEmpty(fs.notes);
}

function renderAllAnnotations() {
  for (const fs of S.order) renderAnnotations(fs);
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
  const fs = S.files.get(a.path);
  if (!fs) {
    toast(`${a.path} is not among the changed files`, 'warn');
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
  await anchorRow(fs, a.side, a.line);
  renderAnnotations(fs);
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
  const fs = S.files.get(c.path);
  if (rerender && fs) renderAnnotations(fs);
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

function setSel(fs, side, from, to) {
  unmarkSel();
  const lo = Math.min(from, to);
  const hi = Math.max(from, to);
  const marked = [];
  for (let l = lo; l <= hi; l++) {
    const tr = fs.rows.get(`${side}:${l}`);
    if (tr) {
      tr.classList.add('sel');
      marked.push(tr);
    }
  }
  S.sel = { fs, side, from, start: lo === hi ? null : lo, line: hi, view: fs.rendered, marked };
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

function onGutterDown(e) {
  const td = e.target.closest('td.ln');
  const tr = td && td.closest('tr.ln-row');
  if (!tr || e.button !== 0) return;
  e.preventDefault(); // no text selection while picking lines
  const fs = fileOf(tr);
  const a = rowAnchor(tr);
  const s = S.sel;
  if (e.shiftKey && s && s.fs === fs && s.side === a.side && s.view === fs.rendered) setSel(fs, a.side, s.from, a.line);
  else setSel(fs, a.side, a.line, a.line);
  S.dragging = true;
  hidePopover();
}

function onGutterOver(e) {
  if (!S.dragging || !S.sel) return;
  const tr = e.target.closest('tr.ln-row');
  if (!tr || !e.target.closest('td.ln') || fileOf(tr) !== S.sel.fs) return;
  const a = rowAnchor(tr);
  if (a.side === S.sel.side) setSel(S.sel.fs, a.side, S.sel.from, a.line);
}

function buildPopover() {
  const label = h('span', { class: 'pop-label' });
  const el = h('div', { class: 'popover', hidden: true },
    label,
    h('button', { class: 'btn small primary', onclick: popComment }, 'Comment'),
    h('button', { class: 'btn small', onclick: popAsk }, 'Ask'));
  S.pop = { el, label };
}

function showPopover() {
  const s = S.sel;
  const tr = s && s.fs.rows.get(`${s.side}:${s.line}`);
  if (!tr) return;
  const sec = s.fs.section.getBoundingClientRect();
  const row = tr.getBoundingClientRect();
  const gutter = tr.cells[s.view === 'diff' ? 1 : 0].getBoundingClientRect();
  S.pop.label.textContent = lineLabel(s);
  S.pop.el.style.top = `${row.bottom - sec.top + 2}px`;
  S.pop.el.style.left = `${gutter.right - sec.left + 8}px`;
  s.fs.section.append(S.pop.el);
  S.pop.el.hidden = false;
}

function hidePopover() {
  if (S.pop) S.pop.el.hidden = true;
}

function popComment() {
  const s = S.sel;
  if (!s) return;
  clearSel();
  openComposer({ path: s.fs.path, side: s.side, line: s.line, start: s.start });
}

function popAsk() {
  const s = S.sel;
  if (!s) return;
  clearSel();
  openChatNew({ path: s.fs.path, side: s.side, line: s.line, start: s.start });
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

function openChatNew(anchor) {
  Object.assign(S.chat, { threadId: null, anchor, thread: null });
  openDrawer('chat');
  $('#chat-input').focus();
}

async function openThread(id) {
  Object.assign(S.chat, { threadId: id, anchor: null });
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
    ? h('button', { class: 'anchor-chip', title: 'Jump to code', onclick: () => revealLine(anchor.path, anchor.side || 'RIGHT', anchor.line) }, anchorText(anchor))
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
  const body = from != null
    ? { thread: from, path: null, start: null, line: null, side: null, question }
    : { thread: null, path: a ? a.path : null, start: a ? a.start ?? null : null, line: a ? a.line : null, side: a ? a.side : null, question };
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
  if (!confirm(`Submit review as "${ev.label}" with ${plural(S.drafts.length, 'draft comment')}?`)) return;
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


// ───────────────────────────── wiring & boot ─────────────────────────────

function wire() {
  const sections = $('#sections');
  sections.addEventListener('mousedown', onGutterDown);
  sections.addEventListener('mouseover', onGutterOver);
  sections.addEventListener('click', e => {
    const marker = e.target.closest('tr.del-marker');
    if (marker) jumpToDeleted(fileOf(marker), +marker.dataset.after);
  });
  document.addEventListener('mouseup', () => {
    if (!S.dragging) return;
    S.dragging = false;
    showPopover();
  });
  document.addEventListener('mousedown', e => {
    if (S.sel && !e.target.closest('.popover') && !e.target.closest('tr.ln-row td.ln')) clearSel();
  });
  document.addEventListener('keydown', e => {
    if (e.key !== 'Escape') return;
    if (S.sel) clearSel();
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
  buildSections();
  buildSidebar();
  setCurrent(S.order[0]);
  const [drafts, threads] = await Promise.all([get('/api/drafts'), get('/api/threads')]);
  S.drafts = drafts || [];
  S.threads = threads || [];
  renderDraftCounts();
  renderAllAnnotations();
}

boot();
