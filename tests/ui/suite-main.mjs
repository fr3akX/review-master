let fails = 0;
const check = (name, ok, extra = '') => { if (!ok) fails++; console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : extra); };
export default async page => {
  const q = (fn, ...a) => page.eval(fn, ...a);
  await page.waitFor(() => document.querySelectorAll('section.file table.code-table').length >= 3);
  await page.waitFor(() => document.querySelector('.card.remote'));
  await q(() => { window.confirm = () => true; });

  // ── load state
  const st = await q(() => ({
    files: document.querySelectorAll('section.file').length,
    viewed: document.querySelector('#viewed-count').textContent,
    remote: document.querySelector('#remote-count').textContent,
    remoteHidden: document.querySelector('#remote-toggle').hidden,
    cards: document.querySelectorAll('.card.remote').length,
  }));
  check('8 files', st.files === 8, JSON.stringify(st));
  check('viewed counter', st.viewed === '0/8 viewed', st.viewed);
  check('PR comments pill', st.remote === 'PR comments · 3' && !st.remoteHidden, JSON.stringify(st));

  // ── intraline marks in handler.py
  const marks = await q(() => [...[...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('handler.py'))
    .querySelectorAll('mark.intra')].map(m => `${m.closest('tr').className.replace('ln-row ', '')}:${m.textContent}`));
  check('word-level marks', marks.includes('del:400') && marks.includes('add:422') && marks.includes('add:"hint"') && !marks.some(m => m.includes('return')), JSON.stringify(marks));

  // ── PR comment threads placement
  const remote = await q(() => [...document.querySelectorAll('.card.remote')].filter(c => c.offsetParent).map(c => ({
    file: c.closest('section.file').querySelector('.file-head .path').textContent,
    comments: c.querySelectorAll('.remote-comment').length,
    inNotes: !!c.closest('.file-notes'), outdated: c.classList.contains('outdated'),
    prevRow: c.closest('tr') ? [c.closest('tr').previousElementSibling.dataset.old, c.closest('tr').previousElementSibling.dataset.new] : null,
    hasRef: !!c.querySelector('a.code-ref'),
  })));
  const cache = remote.find(r => r.file.includes('cache.rs'));
  check('thread with reply on cache.rs:16 RIGHT', cache && cache.comments === 2 && cache.prevRow[1] === '16' && cache.hasRef, JSON.stringify(cache));
  const handler = remote.find(r => r.file.includes('handler.py'));
  check('LEFT thread on handler.py old line 7', handler && handler.prevRow[0] === '7' && handler.prevRow[1] == null, JSON.stringify(handler));
  const ttl = remote.find(r => r.file.includes('ttl.rs'));
  check('outdated thread in notes', ttl && ttl.inNotes && ttl.outdated, JSON.stringify(ttl));
  await page.shot('top');

  // ── viewed via checkbox
  await page.clickSel('section.file .viewed-toggle input');
  let v = await q(() => ({ collapsed: document.querySelector('section.file').classList.contains('collapsed'), count: document.querySelector('#viewed-count').textContent, nav: document.querySelectorAll('#file-list li.viewed').length, bodyShown: !!document.querySelector('section.file .file-body').offsetParent }));
  check('viewed collapses + counts', v.collapsed && v.count === '1/8 viewed' && v.nav === 1 && !v.bodyShown, JSON.stringify(v));

  // ── keyboard: j/k, v, x, n/p, ?, /
  const cur = () => q(() => document.querySelector('#file-list li.active .file-item').title);
  const first = await cur();
  await page.key('j');
  const second = await cur();
  check('j moves to next file', first === 'docs/guide.md' && second === 'src/api/handler.py', `${first} → ${second}`);
  await page.key('k');
  check('k moves back', (await cur()) === 'docs/guide.md', await cur());
  await page.key('j'); await page.key('v');
  v = await q(() => document.querySelector('#viewed-count').textContent);
  check('v marks current viewed', v === '2/8 viewed', v);
  await page.key('v');
  v = await q(() => [document.querySelector('#viewed-count').textContent, document.querySelectorAll('section.file.collapsed').length]);
  check('v again unmarks + expands', v[0] === '1/8 viewed' && v[1] === 1, JSON.stringify(v));
  await page.key('x');
  check('x collapses', (await q(() => document.querySelectorAll('section.file.collapsed').length)) === 2);
  await page.key('x');
  check('x expands', (await q(() => document.querySelectorAll('section.file.collapsed').length)) === 1);
  await page.key('n');
  let ch = await q(() => { const t = document.querySelector('tr.flash'); return t ? [t.closest('section.file').querySelector('.file-head .path').textContent, t.dataset.old || null, t.dataset.new || null] : null; });
  check('n lands on first change of handler.py', ch && ch[0].includes('handler.py') && ch[1] === '7', JSON.stringify(ch));
  await page.key('n');
  ch = await q(() => { const all = [...document.querySelectorAll('tr.flash')]; const t = all[all.length - 1]; return [t.dataset.new]; });
  await page.key('n'); await page.key('p');
  const ch2 = await q(() => window.getComputedStyle(document.body).display);
  check('n/p do not throw', !!ch2);
  await page.key('?', { code: 'Slash' });
  check('? opens help', await q(() => !!document.querySelector('.modal .help-table')));
  await page.shot('help');
  await page.key('Escape', { vk: 27 });
  check('Esc closes help', await q(() => !document.querySelector('.modal')));
  await page.key('/', { code: 'Slash' });
  check('/ focuses filter', await q(() => document.activeElement.id === 'file-filter'));
  await q(() => { const f = document.querySelector('#file-filter'); f.value = 'store rs'; f.dispatchEvent(new Event('input')); });
  let tree = await q(() => [...document.querySelectorAll('#file-list .file-item')].map(b => b.title));
  check('filter narrows tree', JSON.stringify(tree) === JSON.stringify(['src/store/cache.rs', 'src/store/ttl.rs']), JSON.stringify(tree));
  await page.key('Enter', { vk: 13 });
  await page.sleep(200);
  check('Enter jumps to first match', (await cur()) === 'src/store/cache.rs', await cur());
  await q(() => { const f = document.querySelector('#file-filter'); f.focus(); f.value = 'zzz'; f.dispatchEvent(new Event('input')); });
  check('no match message', await q(() => document.querySelector('#file-list').textContent.includes('No files match')));
  await page.key('Escape', { vk: 27 });
  tree = await q(() => document.querySelectorAll('#file-list .file-item').length);
  check('Esc clears filter', tree === 8 && await q(() => document.querySelector('#file-filter').value === ''), String(tree));

  // ── comment: click line number → Comment → save
  await q(() => { const sec = [...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('cache.rs')); sec.querySelector('tr[data-new="17"] td.ln').scrollIntoView({ block: 'center' }); });
  const pt = await q(() => { const sec = [...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('cache.rs')); const b = sec.querySelector('tr[data-new="17"]').cells[1].getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; });
  await page.click(pt.x, pt.y);
  check('popover shown', await q(() => { const p = document.querySelector('.popover'); return p && !p.hidden; }));
  await q(() => document.querySelector('.popover .btn.primary').click());
  await page.waitFor(() => document.querySelector('.composer textarea'));
  await q(() => { const ta = document.querySelector('.composer textarea'); ta.value = 'Counting hits on every read makes `get` take `&mut self`.'; ta.dispatchEvent(new Event('input')); });
  await q(() => document.querySelector('.composer .btn.primary').click());
  await page.waitFor(() => document.querySelector('.card.draft'));
  check('draft saved + badge', await q(() => document.querySelector('#draft-count').textContent === '1'));

  // ── chat: Ask from popover, answer, then slow + Stop
  await page.click(pt.x, pt.y);
  await q(() => [...document.querySelectorAll('.popover .btn')].find(b => b.textContent === 'Ask').click());
  await q(() => { const ta = document.querySelector('#chat-input'); ta.value = 'What does this do?'; ta.dispatchEvent(new Event('input')); document.querySelector('#chat-send').click(); });
  await page.waitFor(() => document.querySelector('#chat-send').textContent === 'Stop', 3000);
  check('send becomes Stop while running', true);
  await page.waitFor(() => document.querySelector('.msg.assistant'), 8000);
  check('answer rendered with code refs', await q(() => document.querySelectorAll('.msg.assistant a.code-ref').length === 2 && document.querySelector('#chat-send').textContent === 'Send'));
  await q(() => { const ta = document.querySelector('#chat-input'); ta.value = 'slow please'; ta.dispatchEvent(new Event('input')); document.querySelector('#chat-send').click(); });
  await page.waitFor(() => document.querySelector('#chat-send').textContent === 'Stop', 3000);
  await page.sleep(500);
  await q(() => document.querySelector('#chat-send').click());
  await page.waitFor(() => document.querySelector('.msg.note'), 5000);
  check('Stop cancels the turn', await q(() => document.querySelector('.msg.note').textContent.includes('Stopped') && document.querySelector('#chat-send').textContent === 'Send' && !document.querySelector('.msg.error')));
  await page.shot('chat');

  // ── hidden context: long.txt has gaps of 1 (top), 38 (middle) and 7 (bottom) lines
  await q(() => document.querySelector('#chat-drawer [data-close]').click());
  await q(() => document.querySelector('#file-list .file-item[title="src/long.txt"]').click());
  await page.waitFor(() => window.longSec || (window.longSec = [...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('long.txt') && s.querySelector('tr.hunk'))));
  const gaps = () => q(() => [...window.longSec.querySelectorAll('tr.hunk')].map(tr => `${tr.dataset.kind}:${tr.dataset.from}-${tr.dataset.to}:${[...tr.querySelectorAll('.gap-btn')].map(b => b.dataset.expand).join('')}`));
  const nums = () => q(() => [...window.longSec.querySelectorAll('tr.ln-row')].filter(r => r.dataset.new).map(r => +r.dataset.new));
  const rising = list => list.every((n, i) => i === 0 || n === list[i - 1] + 1);
  check('gap rows', JSON.stringify(await gaps()) === JSON.stringify(['first:1-1:all', 'mid:9-46:downup', 'tail:54-60:all']), JSON.stringify(await gaps()));
  await q(() => window.longSec.querySelector('tr.hunk[data-kind="mid"] [data-expand="up"]').click());
  await page.waitFor(() => window.longSec.querySelector('tr[data-new="27"]'));
  check('↑ reveals 20 lines above the hunk', JSON.stringify(await gaps()) === JSON.stringify(['first:1-1:all', 'mid:9-26:all', 'tail:54-60:all']), JSON.stringify(await gaps()));
  let ctx = await q(() => { const r = window.longSec.querySelector('tr[data-new="27"]'); return [r.dataset.old, r.cells[3].textContent, r.previousElementSibling.classList.contains('hunk')]; });
  check('revealed line is right and sits below the header', ctx[0] === '27' && ctx[1] === 'line 27 of the long file' && ctx[2], JSON.stringify(ctx));
  await q(() => window.longSec.querySelector('tr.hunk[data-kind="mid"] .link-btn').click());
  await page.waitFor(() => window.longSec.querySelector('tr[data-new="9"]'));
  await q(() => window.longSec.querySelector('tr.hunk[data-kind="tail"] [data-expand="all"]').click());
  await page.waitFor(() => window.longSec.querySelector('tr[data-new="60"]'));
  await q(() => window.longSec.querySelector('tr.hunk[data-kind="first"] [data-expand="all"]').click());
  await page.waitFor(() => window.longSec.querySelector('tr[data-new="1"]'));
  const all = await nums();
  check('whole file revealed in order', all.length === 60 && all[0] === 1 && rising(all) && (await gaps()).length === 0, `${all.length} ${JSON.stringify(await gaps())}`);
  // cache.rs: old and new numbers differ by 2 between its hunks; ↓ grows from the hunk above
  await q(() => document.querySelector('#file-list .file-item[title="src/store/cache.rs"]').click());
  await q(() => { window.cacheSec = [...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('cache.rs')); window.cacheSec.querySelector('tr.hunk[data-kind="mid"] [data-expand="all"]').click(); });
  await page.waitFor(() => window.cacheSec.querySelector('tr[data-new="38"]'));
  ctx = await q(() => { const r = window.cacheSec.querySelector('tr[data-new="38"]'); return [r.dataset.old, r.cells[3].textContent.trim(), !!window.cacheSec.querySelector('tr.fold-bar')]; });
  check('old numbers follow the offset', ctx[0] === '36' && ctx[1] === 'let mut c = Cache::new(2);' && ctx[2], JSON.stringify(ctx));
  await page.shot('expanded');

  // ── reply in a PR thread: the box survives re-renders, the reply shows up in the thread
  await q(() => [...window.cacheSec.querySelectorAll('.card.remote .link-btn')].find(b => b.textContent === 'Reply').click());
  await page.waitFor(() => window.cacheSec.querySelector('.reply-box textarea'));
  await q(() => { const ta = window.cacheSec.querySelector('.reply-box textarea'); ta.value = 'AtomicU64 sounds right.'; ta.dispatchEvent(new Event('input')); });
  await q(() => document.querySelector('#show-remote').click());
  await q(() => document.querySelector('#show-remote').click());
  check('reply text survives re-render', await q(() => window.cacheSec.querySelector('.reply-box textarea').value === 'AtomicU64 sounds right.'));
  await q(() => window.cacheSec.querySelector('.reply-box .btn.primary').click());
  await page.waitFor(() => window.cacheSec.querySelectorAll('.card.remote .remote-comment').length === 3, 5000);
  check('reply posted and listed', await q(() => !window.cacheSec.querySelector('.reply-box') && window.cacheSec.querySelectorAll('.card.remote .remote-comment')[2].textContent.includes('AtomicU64 sounds right.')));

  // ── review drawer: body persists across reload
  await q(() => document.activeElement.blur());
  await page.key('r');
  await page.waitFor(() => document.querySelector('.review-summary'));
  await q(() => { const ta = document.querySelector('.review-summary'); ta.value = 'Looks good overall.'; ta.dispatchEvent(new Event('input')); });
  check('unviewed hint', await q(() => document.querySelector('.review-actions .hint').textContent === '7 files not marked viewed'), await q(() => document.querySelector('.review-actions').textContent));
  await page.shot('review');

  await page.goto(page.url);
  await page.waitFor(() => document.querySelectorAll('section.file').length === 8 && document.querySelector('.card.draft'));
  await q(() => { window.confirm = () => true; });
  const re = await q(() => ({ viewed: document.querySelector('#viewed-count').textContent, collapsed: document.querySelectorAll('section.file.collapsed').length, cur: document.querySelector('#file-list li.active .file-item').title }));
  check('reload keeps viewed + position', re.viewed === '1/8 viewed' && re.collapsed === 1 && re.cur === 'src/store/cache.rs', JSON.stringify(re));
  await page.key('r');
  await page.waitFor(() => document.querySelector('.review-summary'));
  check('reload keeps review summary', await q(() => document.querySelector('.review-summary').value === 'Looks good overall.'));
  await page.key('c');
  await page.waitFor(() => document.querySelector('.thread-row'));
  check('thread listed after reload', await q(() => document.querySelectorAll('.thread-row').length === 1));

  // ── submit, then the form is still usable
  await page.key('r');
  await q(() => document.querySelector('.review-actions .btn.primary').click());
  await page.waitFor(() => document.querySelector('#banner .banner.ok'), 5000);
  const after = await q(() => ({ disabled: document.querySelector('.review-summary').disabled, btn: document.querySelector('.review-actions .btn.primary').disabled, drafts: document.querySelector('#draft-count').hidden, body: document.querySelector('.review-summary').value }));
  check('after submit: form unlocked, drafts cleared', !after.disabled && !after.btn && after.drafts && after.body === '', JSON.stringify(after));

  // ── resizer
  const before = await q(() => document.querySelector('#sidebar').getBoundingClientRect().width);
  await page.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: before, y: 400, button: 'left', clickCount: 1 });
  await page.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: before + 80, y: 400, button: 'left' });
  await page.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: before + 80, y: 400, button: 'left', clickCount: 1 });
  const afterW = await q(() => document.querySelector('#sidebar').getBoundingClientRect().width);
  check('sidebar resizes', Math.abs(afterW - (before + 80)) < 3, `${before} → ${afterW}`);
  if (fails) throw new Error(fails + ' check(s) failed');
};
