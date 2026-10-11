// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

// The map workspace under plain Node with a stand-in DOM and a stand-in
// Leaflet that records what is drawn.

const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const crateDir = process.argv[2];
assert.ok(crateDir, 'crate directory argument is required');
const html = fs.readFileSync(path.join(crateDir, 'assets', 'workspace.html'), 'utf8');
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((match) => match[1]);
assert.strictEqual(scripts.length, 1, 'one inline workspace script');
const source = scripts[0];

function element(id) {
  return {
    id,
    hidden: false,
    innerHTML: '',
    isConnected: true,
    attributes: {},
    setAttribute(name, value) { this.attributes[name] = String(value); },
    removeAttribute(name) { delete this.attributes[name]; },
    querySelector() { return null; },
    querySelectorAll() { return []; },
    addEventListener() {},
    get firstElementChild() { return null; },
  };
}

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function leafletRecorder() {
  const calls = { polyline: [], circleMarker: [], circle: [], fitBounds: 0, tooltips: [] };
  const layer = (kind, args) => {
    const object = {
      kind,
      args,
      addTo() { return object; },
      bindTooltip(node) { calls.tooltips.push(node); return object; },
      on() { return object; },
      getBounds() { return {}; },
    };
    return object;
  };
  const L = {
    map() {
      return { fitBounds() { calls.fitBounds += 1; } };
    },
    control: { scale() { return { addTo() {} }; } },
    polyline(points, style) { calls.polyline.push(points); return layer('polyline', [points, style]); },
    circleMarker(point, style) { calls.circleMarker.push(point); return layer('circleMarker', [point, style]); },
    circle(point, style) { calls.circle.push(point); return layer('circle', [point, style]); },
    featureGroup() { return layer('group', []); },
  };
  return { L, calls };
}

async function run({ pathname, responses, withLeaflet = false }) {
  const elements = {
    'map-shell': element('map-shell'),
    'map-view-root': element('map-view-root'),
    'map-state-gate': element('map-state-gate'),
    'map-canvas': element('map-canvas'),
  };
  const replaced = [];
  const fetched = [];
  const leaflet = withLeaflet ? leafletRecorder() : null;
  const window = {
    AppServices: { escapeHtml },
    SurfaceState: {
      loading: ({ text }) => `<loading>${escapeHtml(text)}</loading>`,
      empty: ({ heading, desc }) => `<empty><h>${escapeHtml(heading)}</h><p>${escapeHtml(desc)}</p></empty>`,
      error: ({ heading }) => `<error>${escapeHtml(heading)}</error>`,
    },
    ConveyIcons: { svg: () => '' },
    CONVEY_COPY: {},
    solPathContext: () => ({ day: /^\/app\/map\/(\d{8})$/.exec(pathname)?.[1] || null }),
    apiJson: async (url) => {
      fetched.push(url);
      if (!(url in responses)) throw new Error(`unexpected fetch ${url}`);
      return responses[url];
    },
    location: { replace: (url) => replaced.push(url) },
    getComputedStyle: () => ({ getPropertyValue: () => '' }),
    L: leaflet ? leaflet.L : undefined,
  };
  const document = {
    getElementById: (id) => elements[id] || null,
    documentElement: {},
    createElement: () => element('created'),
  };
  const context = vm.createContext({ window, document, console, encodeURIComponent, Number, String, Array, Date, Promise });
  vm.runInContext(source, context);
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
  return { elements, replaced, fetched, calls: leaflet?.calls };
}

const time = (clock, day = '20261002') => ({ utc: 'x', clock, day });
const day = {
  day: '20261002',
  devices: [{ phrase: 'your iphone', word: 'iphone', platform: 'ios', tier: 'light', readings: 2, visits: 2,
    first: time('09:00'), last: time('09:05'), interrupted_segments: 0, precision_reduced: false }],
  entries: [
    { kind: 'visit', device: 0, area: { label: 'near Denver, Colorado' }, arrival: time('18:14', '20261001'),
      departure: time('07:44'), point: [39.75, -105.0, 40],
      segments: [{ url: '/app/transcripts/20261002?stream=s#074500_300', segment: '074500_300' }] },
    { kind: 'readings', device: 0, area: { label: 'near <b>Golden</b>, Colorado' }, precision: 'precise',
      first: time('09:00'), last: time('09:05'), count: 2, points: [[39.75, -105.22, 10], [39.76, -105.22, 10]],
      segments: [
        { url: '/app/transcripts/20261002?stream=s#090000_300', segment: '090000_300' },
        { url: '/app/transcripts/20261002?stream=s#090500_300', segment: '090500_300' },
      ] },
    { kind: 'visit', device: 0, area: { label: 'near Denver, Colorado' }, arrival: null, departure: null,
      point: [39.74, -104.99, 40],
      segments: [{ url: '/app/transcripts/20261002?stream=s#180000_300', segment: '180000_300' }] },
    { kind: 'unreadable', device: 0, at: time('11:00'), lines: 1, whole_file: false,
      segments: [{ url: '/app/transcripts/20261002?stream=s#110000_300', segment: '110000_300' }] },
  ],
  map: {
    bounds: [-105.3, 39.7, -104.9, 39.8],
    towns: [{ name: 'Denver', lat: 39.74, lon: -104.98, population: 1 }],
    regions: [{ name: 'Colorado', lines: [[[41, -105], [41, -104]]] }],
    countries: [],
  },
  journal_has_location: true,
  nearby: { previous: '20261001', next: '20261003' },
};

const tests = [];
const test = (name, body) => tests.push({ name, body });

test('every supporting segment is linked from its record', async () => {
  const { elements } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': day } });
  const markup = elements['map-view-root'].innerHTML;
  for (const entry of day.entries) {
    for (const segment of entry.segments) {
      assert.ok(markup.includes(escapeHtml(segment.url)), `missing link ${segment.url}`);
    }
  }
  assert.strictEqual((markup.match(/data-map-entry=/g) || []).length, day.entries.length);
});

test('a visit with no known ends shows neither end', async () => {
  const { elements } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': day } });
  const markup = elements['map-view-root'].innerHTML;
  assert.ok(markup.includes('…–​…'), 'both ends unknown');
  assert.ok(markup.includes('Oct 1 18:14'), 'an arrival on the day before carries its day');
});

test('names are escaped, never markup', async () => {
  const { elements } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': day } });
  const markup = elements['map-view-root'].innerHTML;
  assert.ok(!markup.includes('<b>Golden</b>'));
  assert.ok(markup.includes('&lt;b&gt;Golden&lt;/b&gt;'));
});

test('the map draws every record and never a line between readings', async () => {
  const { calls } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': day }, withLeaflet: true });
  const boundaryLines = day.map.regions.reduce((sum, region) => sum + region.lines.length, 0);
  assert.strictEqual(calls.polyline.length, boundaryLines, 'only borders are lines');
  const recordPoints = day.entries.reduce((sum, entry) => sum + (entry.kind === 'readings' ? entry.points.length : entry.kind === 'visit' ? 1 : 0), 0);
  const townPoints = day.map.towns.length;
  assert.strictEqual(calls.circleMarker.length, recordPoints + townPoints);
  assert.strictEqual(calls.fitBounds, 1);
  assert.deepStrictEqual(calls.tooltips.map((node) => node.textContent), ['Denver'], 'town labels are set as text');
});

test('a day without location offers the nearest days that have it', async () => {
  const empty = { day: '20261002', devices: [], entries: [], map: null, journal_has_location: true, nearby: { previous: '20261001', next: null } };
  const { elements } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': empty } });
  assert.ok(elements['map-state-gate'].innerHTML.includes('<empty>'));
  assert.strictEqual(elements['map-view-root'].hidden, true);
});

test('the bare app opens the latest day with location, or says there is none', async () => {
  const withDays = await run({ pathname: '/app/map/', responses: { '/app/map/api/index': { coverage: { start: '20260702', end: '20261005' }, months: {} } } });
  assert.deepStrictEqual(withDays.replaced, ['/app/map/20261005']);
  const none = await run({ pathname: '/app/map/', responses: { '/app/map/api/index': { coverage: null, months: {} } } });
  assert.deepStrictEqual(none.replaced, []);
  assert.ok(none.elements['map-state-gate'].innerHTML.includes('<empty>'));
  assert.ok('data-map-empty' in none.elements['map-shell'].attributes);
});

(async () => {
  for (const { name, body } of tests) {
    try {
      await body();
    } catch (error) {
      console.error(`FAILED: ${name}\n${error.stack}`);
      process.exit(1);
    }
  }
  console.log(`DOM CASES: ${tests.length} passed`);
})();
