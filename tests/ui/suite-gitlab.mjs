// The same demo change as a GitLab merge request (fake glab): labels, discussions,
// replies, and a review published as draft notes plus an approval.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
let fails = 0;
const check = (name, ok, extra = '') => { if (!ok) fails++; console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : extra); };
export default async page => {
  const q = (fn, ...a) => page.eval(fn, ...a);
  await page.waitFor(() => document.querySelectorAll('section.file table.code-table').length >= 3);
  await page.waitFor(() => document.querySelector('.card.remote'));
  await q(() => { window.confirm = () => true; });

  const head = await q(() => ({ num: document.querySelector('.pr-number').textContent.trim(), title: document.title, remote: document.querySelector('#remote-count').textContent }));
  check('merge request label', head.num === '!1' && head.title.startsWith('!1 '), JSON.stringify(head));
  check('MR comments pill (system and general notes left out)', head.remote === 'MR comments · 3', head.remote);
  const cards = await q(() => [...document.querySelectorAll('.card.remote')].map(c => ({
    file: c.closest('section.file').querySelector('.file-head .path').textContent,
    n: c.querySelectorAll('.remote-comment').length, outdated: c.classList.contains('outdated'),
    prev: c.closest('tr') ? [c.closest('tr').previousElementSibling.dataset.old || null, c.closest('tr').previousElementSibling.dataset.new || null] : null,
    link: c.querySelector('a.link-btn')?.textContent,
  })));
  const cache = cards.find(c => c.file.includes('cache.rs'));
  check('thread with reply on cache.rs:16', cache && cache.n === 2 && cache.prev[1] === '16' && cache.link === 'GitLab ↗', JSON.stringify(cache));
  const handler = cards.find(c => c.file.includes('handler.py'));
  check('LEFT discussion on old line 7', handler && handler.prev[0] === '7' && handler.prev[1] === null, JSON.stringify(handler));
  check('discussion on an older version is outdated', cards.some(c => c.file.includes('ttl.rs') && c.outdated), JSON.stringify(cards));

  // reply in a discussion
  await q(() => { window.cacheSec = [...document.querySelectorAll('section.file')].find(s => s.querySelector('.file-head').textContent.includes('cache.rs')); });
  await q(() => [...window.cacheSec.querySelectorAll('.card.remote .link-btn')].find(b => b.textContent === 'Reply').click());
  await page.waitFor(() => window.cacheSec.querySelector('.reply-box textarea'));
  check('reply button names GitLab', await q(() => window.cacheSec.querySelector('.reply-box .btn.primary').textContent === 'Reply on GitLab'));
  await q(() => { const ta = window.cacheSec.querySelector('.reply-box textarea'); ta.value = 'Agreed, AtomicU64.'; ta.dispatchEvent(new Event('input')); window.cacheSec.querySelector('.reply-box .btn.primary').click(); });
  await page.waitFor(() => window.cacheSec.querySelectorAll('.card.remote .remote-comment').length === 3, 5000);
  check('reply lands in the discussion', true);

  // a draft on an unchanged line (needs both line numbers on GitLab), then review: approve
  const pt = await q(() => { const tr = window.cacheSec.querySelector('tr[data-new="17"]'); tr.scrollIntoView({ block: 'center' }); const b = tr.cells[1].getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; });
  await page.click(pt.x, pt.y);
  await q(() => document.querySelector('.popover .btn.primary').click());
  await page.waitFor(() => document.querySelector('.composer textarea'));
  await q(() => { const ta = document.querySelector('.composer textarea'); ta.value = 'Reads now need exclusive access.'; ta.dispatchEvent(new Event('input')); document.querySelector('.composer .btn.primary').click(); });
  await page.waitFor(() => document.querySelector('.card.draft'));
  await q(() => document.activeElement.blur());
  await page.key('r');
  await page.waitFor(() => document.querySelector('.review-summary'));
  const radios = await q(() => Object.fromEntries([...document.querySelectorAll('input[name="review-event"]')].map(r => [r.value, r.disabled])));
  check('request changes is off on GitLab, approve on', radios.request_changes && !radios.approve && !radios.comment, JSON.stringify(radios));
  await q(() => {
    const ta = document.querySelector('.review-summary'); ta.value = 'Looks good apart from the reader lock.'; ta.dispatchEvent(new Event('input'));
    const ap = document.querySelector('input[name="review-event"][value="approve"]'); ap.checked = true; ap.dispatchEvent(new Event('change'));
    document.querySelector('.review-actions .btn.primary').click();
  });
  await page.waitFor(() => document.querySelector('#banner .banner.ok'), 5000);
  check('submitted banner links the merge request', await q(() => document.querySelector('#banner a').href.endsWith('/-/merge_requests/1')));
  const review = JSON.parse(readFileSync(join(dirname(process.env.FAKE_GH_OUT), 'gitlab-review.json'), 'utf8'));
  const inline = review.drafts.find(d => d.position);
  const general = review.drafts.find(d => !d.position);
  check('inline draft note carries both lines of the unchanged line', inline && inline.position.new_line === 17 && inline.position.old_line === 15 && inline.position.new_path === 'src/store/cache.rs' && inline.note.includes('exclusive access'), JSON.stringify(inline));
  check('summary as a general note', general && general.note.startsWith('Looks good'), JSON.stringify(general));
  check('published and approved at the head', review.published && /^[0-9a-f]{40}$/.test(review.approved || ''), JSON.stringify(review));
  await page.shot('gitlab');
  if (fails) throw new Error(fails + ' check(s) failed');
};
