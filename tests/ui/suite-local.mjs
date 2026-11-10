let fails = 0;
const check = (name, ok, extra = '') => { if (!ok) fails++; console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : extra); };
export default async page => {
  const q = (fn, ...a) => page.eval(fn, ...a);
  await page.waitFor(() => document.querySelectorAll('section.file').length === 8);
  await page.sleep(800);
  check('local mode header', await q(() => document.querySelector('#pr-title').textContent.startsWith('Local review')));
  check('no PR comments UI', await q(() => document.querySelector('#remote-toggle').hidden && document.querySelector('#remote-retry').hidden));
  await page.key('r');
  await page.waitFor(() => document.querySelector('.review-actions .btn.primary'));
  check('submit disabled in local mode', await q(() => document.querySelector('.review-actions .btn.primary').disabled));
  await page.shot('local');
  if (fails) throw new Error(fails + ' failed');
};
