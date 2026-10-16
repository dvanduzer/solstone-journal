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
const curationEvidenceSource = fs.readFileSync(
  path.join(manifestDir, 'assets/curation/curation_evidence.js'),
  'utf8'
);

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

function createCurationEnvironment() {
  const windowObj = {
    AppServices: { escapeHtml: (s) => String(s ?? '') },
    location: { search: '' },
    addEventListener() {},
  };
  const context = vm.createContext({
    window: windowObj,
    console,
    Math,
    Number,
    String,
    Object,
    Array,
    Boolean,
  });

  vm.runInContext(dateFormatSource, context);
  vm.runInContext(curationEvidenceSource, context);

  return { context, window: windowObj };
}

test('curation evidence linked sample renders seeded stream label with raw name in title attribute', () => {
  const env = createCurationEnvironment();
  env.window.JournalFormat.installStreamLabels({
    status: 'complete',
    labels: { ja1r: 'your mac · screen' },
  });

  const props = env.window.CurationEvidence.buildEvidenceDrawerProps({
    kind: 'activity',
    key: 'act-1',
    evidence: {
      count: 1,
      samples: [
        { day: '20260403', segment: '120000_300', stream: 'ja1r' },
      ],
    },
  });

  assert.ok(props, 'props object returned');
  const spanMatch = props.bodyHtml.match(/<span[^>]*class="ev-meta"[^>]*title="([^"]*)"[^>]*>([^<]*)<\/span>/);
  assert.ok(spanMatch, 'ev-meta span found in bodyHtml');
  assert.strictEqual(spanMatch[1], 'ja1r', 'title is raw stream name');
  assert.strictEqual(spanMatch[2], 'your mac · screen', 'span text is seeded label');
});

test('curation evidence plain sample renders seeded stream label with raw name in title attribute', () => {
  const env = createCurationEnvironment();
  env.window.JournalFormat.installStreamLabels({
    status: 'complete',
    labels: { ja1r: 'your mac · screen' },
  });

  const props = env.window.CurationEvidence.buildEvidenceDrawerProps({
    kind: 'entity',
    key: 'ent-1',
    evidence: {
      count: 1,
      samples: [
        { day: '20260403', stream: 'ja1r' }, // missing segment -> plainRow
      ],
    },
  });

  assert.ok(props, 'props object returned');
  const spanMatch = props.bodyHtml.match(/<span[^>]*title="([^"]*)"[^>]*>([^<]*)<\/span>/);
  assert.ok(spanMatch, 'span with title found in bodyHtml');
  assert.strictEqual(spanMatch[1], 'ja1r', 'title is raw stream name');
  assert.strictEqual(spanMatch[2], 'your mac · screen', 'span text is seeded label');
});

console.log(`DOM CASES: ${cases} passed`);
