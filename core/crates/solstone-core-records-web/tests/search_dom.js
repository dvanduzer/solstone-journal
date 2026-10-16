// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const manifestDir = process.argv[2];
assert.ok(manifestDir, 'manifest directory argument is required');

const dateFormatSource = fs.readFileSync(
  path.resolve(manifestDir, '../solstone-core-convey-shell/assets/static/date_format.js'),
  'utf8'
);
const searchHtml = fs.readFileSync(
  path.join(manifestDir, 'assets/search.html'),
  'utf8'
);

class Node {
  constructor(tagName) {
    this.tagName = tagName ? tagName.toUpperCase() : '';
    this.children = [];
    this.attributes = {};
    this._textContent = '';
    this.nodeType = tagName ? 1 : 3;
    this.childNodes = this.children;
  }
  get textContent() {
    if (this.children.length > 0) return this.children.map((c) => c.textContent).join('');
    return this._textContent;
  }
  set textContent(v) {
    this.children = [];
    this._textContent = String(v);
  }
  get className() {
    return this.attributes.class || '';
  }
  set className(v) {
    this.attributes.class = String(v);
  }
  setAttribute(k, v) {
    this.attributes[k] = String(v);
  }
  getAttribute(k) {
    return Object.prototype.hasOwnProperty.call(this.attributes, k) ? this.attributes[k] : null;
  }
  append(...nodes) {
    for (const n of nodes) {
      if (typeof n === 'string') {
        const textNode = new Node('');
        textNode.textContent = n;
        this.children.push(textNode);
      } else if (n && n.nodeType === 11) {
        this.children.push(...n.children);
      } else if (n) {
        this.children.push(n);
      }
    }
  }
  appendChild(child) {
    this.append(child);
    return child;
  }
  querySelectorAll(sel) {
    const results = [];
    const walk = (el) => {
      for (const child of el.children) {
        const isTag = child.tagName && child.tagName.toLowerCase() === sel.toLowerCase();
        const isClass =
          sel.startsWith('.') &&
          child.attributes.class &&
          child.attributes.class.split(' ').includes(sel.slice(1));
        if (isTag || isClass) results.push(child);
        walk(child);
      }
    };
    walk(this);
    return results;
  }
  querySelector(sel) {
    return this.querySelectorAll(sel)[0] || new Node('div');
  }
  addEventListener() {}
}

let cases = 0;
function test(name, fn) {
  cases += 1;
  try {
    fn();
  } catch (err) {
    console.error(`FAILED: ${name}`);
    throw err;
  }
}

function createSearchEnvironment() {
  const location = { search: '' };
  const windowObj = { location, addEventListener() {} };
  const doc = {
    createElement: (tag) => new Node(tag),
    createTextNode: (t) => {
      const n = new Node('');
      n.textContent = t;
      return n;
    },
    createDocumentFragment: () => {
      const f = new Node('');
      f.nodeType = 11;
      return f;
    },
    getElementById: () => new Node('div'),
  };

  class DOMParser {
    parseFromString(str) {
      const parsedDoc = { body: new Node('body') };
      parsedDoc.body.textContent = str.replace(/<[^>]+>/g, '');
      return parsedDoc;
    }
  }

  const context = vm.createContext({
    window: windowObj,
    document: doc,
    DOMParser,
    URLSearchParams,
    location,
    console,
    Math,
    Number,
    String,
    Object,
    Array,
    Boolean,
  });

  vm.runInContext(dateFormatSource, context);

  const scriptStart = searchHtml.indexOf('<script>') + '<script>'.length;
  const scriptEnd = searchHtml.indexOf('</script>', scriptStart);
  let script = searchHtml.slice(scriptStart, scriptEnd);
  script = script.replace(
    'function renderHit(hit,dayHref){',
    'window.renderHit = renderHit;\n function renderHit(hit,dayHref){'
  );
  vm.runInContext(script, context);

  return { context, window: windowObj };
}

test('renderHit renders seeded stream label with raw name in title attribute', () => {
  const env = createSearchEnvironment();
  env.window.JournalFormat.installStreamLabels({
    status: 'complete',
    labels: { ja1r: 'your mac · screen' },
  });

  const hit = {
    stream: 'ja1r',
    text: 'searching across time',
    ts: 1775199600000,
    path: 'chronicle/20260403/ja1r/120000_300/screen.jsonl',
    facet_title: 'personal',
  };

  const card = env.window.renderHit(hit, '/app/transcripts/20260403');
  const meta = card.querySelector('.search-meta');
  assert.ok(meta, 'search-meta element exists');

  const spans = meta.querySelectorAll('span');
  const streamSpan = spans.find((s) => s.getAttribute('title') === 'ja1r');
  assert.ok(streamSpan, 'stream span with title=ja1r exists');
  assert.strictEqual(streamSpan.textContent, 'your mac · screen');
  assert.strictEqual(streamSpan.getAttribute('title'), 'ja1r');
});

test('renderHit suppresses agent label when subsumed by stream label', () => {
  const env = createSearchEnvironment();
  env.window.JournalFormat.installStreamLabels({
    status: 'complete',
    labels: { ja1r: 'your mac · screen' },
  });

  const hit = {
    stream: 'ja1r',
    agent_label: 'screen',
    text: 'matching content',
    ts: 1775199600000,
    path: 'chronicle/20260403/ja1r/120000_300/screen.jsonl',
  };

  const card = env.window.renderHit(hit, '/app/transcripts/20260403');
  const meta = card.querySelector('.search-meta');
  const spans = meta.querySelectorAll('span');
  const agentSpan = spans.find((s) => s.textContent === 'screen' && !s.getAttribute('title'));
  assert.strictEqual(agentSpan, undefined, 'agent label is omitted when stream label subsumes it');
});

test('renderHit keeps agent label for distinct agent and stream', () => {
  const env = createSearchEnvironment();
  env.window.JournalFormat.installStreamLabels({
    status: 'complete',
    labels: { ja1r: 'your mac · screen' },
  });

  const hit = {
    stream: 'ja1r',
    agent_label: 'describe',
    text: 'vision summary',
    ts: 1775199600000,
    path: 'chronicle/20260403/ja1r/120000_300/screen.jsonl',
  };

  const card = env.window.renderHit(hit, '/app/transcripts/20260403');
  const meta = card.querySelector('.search-meta');
  const spans = meta.querySelectorAll('span');
  const agentSpan = spans.find((s) => s.textContent === 'describe');
  assert.ok(agentSpan, 'agent label describe is rendered');
});

console.log(`DOM CASES: ${cases} passed`);
