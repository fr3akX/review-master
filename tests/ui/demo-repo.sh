#!/bin/bash
# Builds the demo repository the UI tests review: `main` plus a `feature/ttl-cache` branch
# that modifies, adds, deletes and renames files. usage: demo-repo.sh <dir>
set -euo pipefail
R=$1
rm -rf "$R" && mkdir -p "$R" && cd "$R"
git init -q -b main . && git config user.email t@example.com && git config user.name Tester
git remote add origin https://github.com/acme/demo.git
mkdir -p src/store src/api tests docs
cat > src/store/cache.rs <<'RS'
//! A tiny cache.
use std::collections::HashMap;

pub struct Cache {
    items: HashMap<String, String>,
    capacity: usize,
}

impl Cache {
    pub fn new(capacity: usize) -> Cache {
        Cache { items: HashMap::new(), capacity }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.items.get(key)
    }

    pub fn put(&mut self, key: String, value: String) {
        if self.items.len() >= self.capacity {
            self.items.clear();
        }
        self.items.insert(key, value);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get() {
        let mut c = Cache::new(2);
        c.put("a".into(), "1".into());
        assert_eq!(c.get("a"), Some(&"1".to_string()));
    }
}
RS
cat > src/api/handler.py <<'PY'
import json


def handle(request, store):
    key = request.get("key")
    if key is None:
        return {"status": 400, "error": "missing key"}
    value = store.get(key)
    if value is None:
        return {"status": 404, "error": "not found"}
    return {"status": 200, "body": json.dumps({"key": key, "value": value})}


def health():
    return {"status": 200}
PY
printf 'def old_handle(request):\n    return {"status": 200}\n' > src/api/legacy.py
cat > src/util.js <<'JS'
export function clamp(value, min, max) {
  return Math.min(Math.max(value, min), max);
}

export function sum(list) {
  let total = 0;
  for (const x of list) total += x;
  return total;
}
JS
printf 'from src.api.handler import handle\n\n\ndef test_missing_key():\n    assert handle({}, {})["status"] == 400\n' > tests/test_handler.py
printf '# Demo\n\nA demo service.\n' > docs/guide.md
for i in $(seq 1 60); do echo "line $i of the long file" >> src/long.txt; done
git add -A && git commit -q -m base
git checkout -q -b feature/ttl-cache
python3 - <<'PY'
p = 'src/store/cache.rs'; s = open(p).read()
s = s.replace("    capacity: usize,\n}", "    capacity: usize,\n    hits: u64,\n}")
s = s.replace("Cache { items: HashMap::new(), capacity }", "Cache { items: HashMap::with_capacity(capacity), capacity, hits: 0 }")
s = s.replace("    pub fn get(&self, key: &str) -> Option<&String> {\n        self.items.get(key)", "    pub fn get(&mut self, key: &str) -> Option<&String> {\n        self.hits += 1;\n        self.items.get(key)")
s = s.replace("        assert_eq!(c.get(\"a\"), Some(&\"1\".to_string()));\n    }", "        assert_eq!(c.get(\"a\"), Some(&\"1\".to_string()));\n    }\n\n    #[test]\n    fn evicts_when_full() {\n        let mut c = Cache::new(1);\n        c.put(\"a\".into(), \"1\".into());\n        c.put(\"b\".into(), \"2\".into());\n        assert_eq!(c.len(), 1);\n    }")
open(p, 'w').write(s)
p = 'src/api/handler.py'; s = open(p).read()
s = s.replace('return {"status": 400, "error": "missing key"}', 'return {"status": 422, "error": "missing key", "hint": "pass ?key="}')
s = s.replace('    value = store.get(key)\n', '    value = store.get(key.strip())\n')
s += '\n\ndef ready(store):\n    return {"status": 200 if store is not None else 503}\n'
open(p, 'w').write(s)
p = 'src/long.txt'; lines = open(p).read().split('\n')
lines[4] = 'line 5 of the LONG file'; lines[49] = 'line 50 of the long file, edited'
open(p, 'w').write('\n'.join(lines))
PY
git rm -q src/api/legacy.py
git mv src/util.js src/math.js
python3 -c "p='src/math.js'; s=open(p).read(); open(p,'w').write(s.replace('let total = 0;', 'let total = 0; // running total'))"
cat > src/store/ttl.rs <<'RS'
use std::time::{Duration, Instant};

pub struct Ttl {
    born: Instant,
    ttl: Duration,
}

impl Ttl {
    pub fn new(ttl: Duration) -> Ttl {
        Ttl { born: Instant::now(), ttl }
    }

    pub fn expired(&self) -> bool {
        self.born.elapsed() > self.ttl
    }
}
RS
printf '\n\ndef test_not_found():\n    assert handle({"key": "x"}, {})["status"] == 404\n' >> tests/test_handler.py
printf '\n## TTL\n\nEntries expire after a TTL.\n' >> docs/guide.md
git add -A && git commit -q -m "ttl cache"
