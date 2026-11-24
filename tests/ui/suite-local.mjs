import { readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
let fails = 0;
const check = (name, ok, extra = '') => { if (!ok) fails++; console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : extra); };
export default async page => {
  const q = (fn, ...a) => page.eval(fn, ...a);
  const work = dirname(dirname(process.env.FAKE_GH_OUT));
  const demo = join(work, 'demo');
  // Survives the page reloading under it.
  const settle = async (fn, ms = 10000) => {
    const t0 = Date.now();
    for (;;) {
      try { if (await q(fn)) return true; } catch { /* navigating */ }
      if (Date.now() - t0 > ms) return false;
      await page.sleep(150);
    }
  };
  await page.waitFor(() => document.querySelectorAll('section.file').length === 8);
  await page.sleep(800);
  check('local mode header', await q(() => document.querySelector('#pr-title').textContent.startsWith('Local review')));
  check('header names the base', await q(() => document.querySelector('#pr-meta').textContent.includes('against main')), await q(() => document.querySelector('#pr-meta').textContent));
  check('no PR comments UI', await q(() => document.querySelector('#remote-toggle').hidden && document.querySelector('#remote-retry').hidden));

  // ── Reload changes: an edit and an untracked file show up; drafts stay
  await q(() => fetch('/api/draft', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ path: 'src/store/cache.rs', line: 17, side: 'RIGHT', severity: 'major', body: '**Count hits atomically.**\n\nUse an `AtomicU64`.' }) }));
  await q(() => document.querySelector('#btn-reload').click());
  await page.waitFor(() => document.querySelector('.toast') && document.querySelector('.toast').textContent.includes('No changes'));
  check('Reload without edits says so', true);
  writeFileSync(join(demo, 'NOTES.md'), '# Notes\n\nAn untracked file.\n');
  const handler = join(demo, 'src/api/handler.py');
  const original = readFileSync(handler, 'utf8');
  writeFileSync(handler, original + '\n# edited while reviewing\n');
  await q(() => { window.__beforeReload = true; document.querySelector('#btn-reload').click(); });
  const reloaded = await settle(() => !window.__beforeReload && document.querySelectorAll('section.file').length === 9);
  check('Reload re-reads the worktree (untracked file added)', reloaded && await q(() => [...document.querySelectorAll('#file-list .file-item')].some(b => b.title === 'NOTES.md')),
    await q(() => [...document.querySelectorAll('#file-list .file-item')].map(b => b.title).join()));
  await page.waitFor(() => document.querySelector('.card.draft'));
  check('drafts survive a reload', await q(() => document.querySelectorAll('.card.draft').length === 1));

  // ── Save review: Markdown in the cache directory, in the findings shape
  await q(() => { window.confirm = () => true; });
  await page.key('r');
  await page.waitFor(() => document.querySelector('.review-actions .btn.primary'));
  check('local review drawer saves instead of submitting', await q(() => document.querySelector('.review-actions .btn.primary').textContent === 'Save review' && !document.querySelector('.review-actions .btn.primary').disabled && !document.querySelector('input[name="review-event"]')));
  await q(() => { const ta = document.querySelector('.review-summary'); ta.value = 'Close, two things.'; ta.dispatchEvent(new Event('input')); });
  await page.shot('local');
  await q(() => document.querySelector('.review-actions .btn.primary').click());
  await page.waitFor(() => document.querySelector('#banner .banner.ok'));
  const saved = join(work, 'cache-local', 'review-feature-ttl-cache.md');
  let md = '';
  try { md = readFileSync(saved, 'utf8'); } catch (e) { md = String(e); }
  check('review saved as Markdown', md.startsWith('# Review of `feature/ttl-cache` (against `main`') && md.includes('Close, two things.') &&
    md.includes('### [major] Count hits atomically. (src/store/cache.rs:17)\n\n**Count hits atomically.**\n\nUse an `AtomicU64`.'), md);
  check('banner shows where', await q(p => document.querySelector('#banner').textContent.includes('Review saved') && document.querySelector('#banner').textContent.includes(p), saved));
  check('saved drafts are done', await q(() => !document.querySelector('.card.draft')));
  await page.key('Escape', { vk: 27 });

  rmSync(join(demo, 'NOTES.md'));
  writeFileSync(handler, original);
  await q(() => document.querySelector('#btn-exit').click());
  await page.waitFor(() => document.querySelector('.stopped'), 5000);
  check('Exit shows the stopped screen', await q(() => document.title === 'review-master (stopped)'));
  check('Exit stops the server', await page.waitFor(() => fetch('/api/info').then(() => false, () => true), 5000));
  if (fails) throw new Error(fails + ' failed');
};
