// Unit checks for the pure helpers of web/app.js, lifted out of the file by name.
import { readFileSync } from 'node:fs';
const src = readFileSync(new URL('../../web/app.js', import.meta.url), 'utf8');
const between = (from, to) => src.slice(src.indexOf(from), src.indexOf(to));
const code = between('const WORD_TOKENS', 'const EXPAND_STEP') + between('const HUNK_RE', '// Cells of a gap row');
const { intraline, markRanges, intraMarks, hunkSpan } = new Function(`${code}; return { intraline, markRanges, intraMarks, hunkSpan };`)();
const esc = s => s.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
let fails = 0;
const eq = (name, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) fails++;
  console.log(ok ? 'ok  ' : 'FAIL', name, ok ? '' : `\n   got  ${JSON.stringify(got)}\n   want ${JSON.stringify(want)}`);
};
const cut = (text, ranges) => ranges.map(([s, e]) => text.slice(s, e));

let a = '    return {"status": 400, "error": "missing key"}';
let b = '    return {"status": 422, "error": "missing key", "hint": "pass ?key="}';
let r = intraline(a, b);
eq('number change', cut(a, r[0]), ['400']);
eq('number + appended', cut(b, r[1]), ['422', ', "hint": "pass ?key="']);
a = '    pub fn get(&self, key: &str) -> Option<&String> {';
b = '    pub fn get(&mut self, key: &str) -> Option<&String> {';
r = intraline(a, b);
eq('pure insertion: nothing on the old side', cut(a, r[0]), []);
eq('pure insertion: new side', cut(b, r[1]), ['mut ']);
eq('unrelated lines → null', intraline('let total = 0;', 'return handle(request)'), null);
eq('identical → null', intraline('x', 'x'), null);
eq('too long → null', intraline('a'.repeat(601), 'b'), null);
r = intraline('foo(a, b)', 'foo(x, y)');
eq('edits around kept punctuation stay separate', cut('foo(x, y)', r[1]), ['x', 'y']);
r = intraline('take x y now', 'take p q now');
eq('adjacent edits merge across a kept blank', [cut('take x y now', r[0]), cut('take p q now', r[1])], [['x y'], ['p q']]);

// markRanges over highlighted html with entities and nested spans
const raw = 'if a < b && c: "x"';
const html = '<span class="k">if</span> a &lt; b &amp;&amp; c: <span class="s">&quot;x&quot;</span>';
const strip = h => h.replace(/<[^>]*>/g, '').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
const marked = (h, ranges) => [...markRanges(h, ranges).matchAll(/<mark class="intra">(.*?)<\/mark>/g)].map(m => strip(m[1])).join('');
eq('markRanges keeps the text', strip(markRanges(html, [[3, 8], [15, 18]])), raw);
eq('markRanges marks the right characters', marked(html, [[3, 8], [15, 18]]), raw.slice(3, 8) + raw.slice(15, 18));
eq('markRanges across a span boundary', marked(html, [[0, 4]]), 'if a');
const whole = markRanges(html, [[0, 18]]);
eq('markRanges balances its tags', (whole.match(/<mark/g) || []).length, (whole.match(/<\/mark>/g) || []).length);
eq('markRanges without ranges', markRanges(html, []), html);
eq('markRanges on escaped text', marked(esc('a<b>&c'), [[1, 4]]), '<b>');

// pairing of deleted/added runs
const L = (k, text) => ({ k, text });
eq('equal runs pair up', [...intraMarks([L('ctx', 'x'), L('del', 'a = 1'), L('del', 'b = 2'), L('add', 'a = 10'), L('add', 'b = 20')]).keys()], [1, 3, 2, 4]);
eq('unequal runs do not pair', intraMarks([L('del', 'a = 1'), L('add', 'a = 10'), L('add', 'c = 3')]).size, 0);
eq('separated runs do not pair', intraMarks([L('del', 'a = 1'), L('ctx', 'z'), L('add', 'a = 10')]).size, 0);

// hunk headers
eq('hunk span', hunkSpan('@@ -4,14 +4,16 @@ use x;'), { os: 4, oe: 17, ns: 4, ne: 19 });
eq('hunk span, counts omitted', hunkSpan('@@ -3 +3 @@'), { os: 3, oe: 3, ns: 3, ne: 3 });
eq('new file', hunkSpan('@@ -0,0 +1,16 @@'), { os: 1, oe: 0, ns: 1, ne: 16 });
eq('pure deletion starts after its anchor line', hunkSpan('@@ -5,2 +4,0 @@'), { os: 5, oe: 6, ns: 5, ne: 4 });
eq('not a hunk header', hunkSpan('@@@ combined'), null);
process.exit(fails ? 1 : 0);
