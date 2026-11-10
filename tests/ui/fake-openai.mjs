// Fake OpenAI-compatible server: answers every chat completion with a JSON answer.
import http from 'node:http';
const port = +process.argv[2] || 8765;
http.createServer((req, res) => {
  let body = '';
  req.on('data', c => (body += c));
  req.on('end', async () => {
    res.setHeader('Content-Type', 'application/json');
    if (req.url.endsWith('/models')) return res.end(JSON.stringify({ data: [{ id: 'fake' }] }));
    const j = JSON.parse(body || '{}');
    const last = [...(j.messages || [])].reverse().find(m => m.role === 'user');
    const text = String(last && last.content || '');
    const isSummary = text.includes('Write the summary a reviewer');
    const isFlows = text.includes('Map the flows this change affects');
    if (/slow/i.test(text.split('Question:').pop())) await new Promise(r => setTimeout(r, 60000));
    else await new Promise(r => setTimeout(r, isSummary || isFlows ? 300 : 600));
    const step = (path, line, end_line, kind, title, change = '') => ({ path, line, end_line, kind, title, detail: `${title}, in detail.`, change });
    const answer = isFlows
      ? { flows: [{ name: 'Handle a key request', description: 'A request with a key reads the cache; a missing key is now a 422.', steps: [
        step('src/api/handler.py', 4, 5, 'entry', 'Request arrives'),
        step('src/api/handler.py', 6, 7, 'branch', 'Missing key is rejected', 'Now 422 with a hint instead of 400.'),
        step('src/store/cache.rs', 16, 18, 'call', 'Cache lookup counts a hit', '`get` takes `&mut self` and counts hits.'),
        step('src/math.js', 1, 3, 'transform', 'Clamps the value'),
        step('src/nowhere.rs', 1, 2, 'return', 'A step the model made up'),
      ] }] }
      : isSummary
        ? { summary: '## Why the change\nCache entries can now expire and hits are counted.\n## Special things to note\n- `get` takes `&mut self` now, see `src/store/cache.rs:16`.\n## Change outline\n```\nsrc/store/ttl.rs   new Ttl type\nsrc/store/cache.rs hit counter\n```\n' }
        : { answer: 'It counts hits in `src/store/cache.rs:17` and the TTL lives in `src/store/ttl.rs:3-6`.\n\n```rust\nself.hits += 1;\n```' };
    res.end(JSON.stringify({ choices: [{ finish_reason: 'stop', message: { role: 'assistant', content: JSON.stringify(answer) } }], usage: {} }));
  });
}).listen(port, '127.0.0.1', () => console.log('fake openai on ' + port));
