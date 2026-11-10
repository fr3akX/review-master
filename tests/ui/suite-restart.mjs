let fails = 0;
const check = (name, ok, extra = '') => { if (!ok) fails++; console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : extra); };
export default async page => {
  const q = (fn, ...a) => page.eval(fn, ...a);
  await page.waitFor(() => document.querySelectorAll('section.file').length === 8);
  await q(() => { window.confirm = () => true; });
  await page.waitFor(() => !document.querySelector('#remote-retry').hidden, 5000);
  check('comments failure → retry link, no cards', await q(() => document.querySelectorAll('.card.remote').length === 0 && document.querySelector('#remote-toggle').hidden));

  // threads persisted across the server restart
  await page.key('c');
  await page.waitFor(() => document.querySelector('.thread-row'), 4000);
  await q(() => document.querySelector('.thread-row .thread-item').click());
  await page.waitFor(() => document.querySelector('.msg.assistant'), 4000);
  const msgs = await q(() => [...document.querySelectorAll('#messages .msg')].map(m => m.className.replace('msg ', '').split(' ')[0] + ':' + m.textContent.slice(0, 18)));
  check('thread history survives restart', msgs.length === 3 && msgs[0].startsWith('user:What') && msgs[1].startsWith('assistant:') && msgs[2].startsWith('user:slow'), JSON.stringify(msgs));
  check('chat chip on the anchored line', await q(() => !!document.querySelector('.thread-chip')));
  // follow-up in the restored session
  await q(() => { const ta = document.querySelector('#chat-input'); ta.value = 'And then?'; ta.dispatchEvent(new Event('input')); document.querySelector('#chat-send').click(); });
  await page.waitFor(() => document.querySelectorAll('.msg.assistant').length === 2, 8000);
  check('follow-up answered in restored session', true);
  // delete thread
  await q(() => document.querySelector('.thread-row .icon-btn').click());
  await page.waitFor(() => !document.querySelector('.thread-row'), 4000);
  check('thread deleted', await q(() => !document.querySelector('.thread-chip') && document.querySelector('#thread-list').textContent.includes('No conversations')));
  await page.key('Escape', { vk: 27 });

  // exclude tests + folds still work; viewed total follows scope
  await q(() => document.querySelector('#hide-tests').click());
  await page.waitFor(() => document.querySelectorAll('section.file').length === 7);
  check('viewed total follows scope', await q(() => document.querySelector('#viewed-count').textContent === '0/7 viewed'), await q(() => document.querySelector('#viewed-count').textContent));
  await page.waitFor(() => document.querySelector('tr.fold-bar.closed'), 5000);
  check('test fold closed when tests excluded', true);
  await q(() => document.querySelector('#hide-tests').click());
  await page.waitFor(() => document.querySelectorAll('section.file').length === 8);

  // full file viewer + o shortcut, keys inside viewer do not act on the diff
  await q(() => document.querySelector('#file-list .file-item[title="src/store/ttl.rs"]').click());
  await page.sleep(300);
  await page.key('o');
  await page.waitFor(() => !document.querySelector('#viewer').hidden && document.querySelector('#viewer table.code-table'));
  const before = await q(() => document.querySelector('#viewed-count').textContent);
  await page.key('v');
  check('v ignored while viewer open', (await q(() => document.querySelector('#viewed-count').textContent)) === before);
  await page.shot('viewer-dark');
  await page.key('Escape', { vk: 27 });
  check('Esc closes viewer', await q(() => document.querySelector('#viewer').hidden));

  // jumping to a line inside a collapsed (viewed) file expands it
  await q(() => document.querySelector('#file-list .file-item[title="src/store/cache.rs"]').click());
  await page.sleep(300);
  await page.key('v');
  check('cache.rs collapsed', await q(() => [...document.querySelectorAll('section.file.collapsed .file-head')].some(h => h.textContent.includes('cache.rs'))));
  await page.waitFor(() => document.querySelector('.summary-text a.code-ref'), 6000);
  await q(() => document.querySelector('.summary-text a.code-ref').click());
  await page.sleep(500);
  const ex = await q(() => ({ collapsed: [...document.querySelectorAll('section.file.collapsed .file-head')].some(h => h.textContent.includes('cache.rs')), flash: !!document.querySelector('tr.flash[data-new="16"]'), viewed: document.querySelector('#viewed-count').textContent }));
  check('code ref expands collapsed file and flashes line', !ex.collapsed && ex.flash, JSON.stringify(ex));
  await page.shot('dark');

  // mark everything viewed → progress done
  await q(() => { for (const b of document.querySelectorAll('section.file .viewed-toggle input')) if (!b.checked) b.click(); });
  check('all viewed', await q(() => document.querySelector('#viewed-count').textContent === '8/8 viewed' && document.querySelector('#viewed-progress').classList.contains('done')));
  await page.key('k'); await page.key('k');
  await page.shot('allviewed-dark');
  // j walks through all collapsed files to the last one
  for (let i = 0; i < 10; i++) await page.key('j');
  check('j reaches the last file', await q(() => document.querySelector('#file-list li.active .file-item').title === 'tests/test_handler.py'), await q(() => document.querySelector('#file-list li.active .file-item').title));
  if (fails) throw new Error(fails + ' check(s) failed');
};
