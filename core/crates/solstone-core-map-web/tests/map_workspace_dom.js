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
  const calls = { polyline: [], polygon: [], circleMarker: [], circle: [], fitBounds: 0, tooltips: [] };
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
    polygon(rings, style) { calls.polygon.push(rings); return layer('polygon', [rings, style]); },
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
    location: { pathname, replace: (url) => replaced.push(url) },
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

test('the map draws every record, a precise run as one dot, and never a line between readings', async () => {
  const { calls } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': day }, withLeaflet: true });
  const boundaryLines = day.map.regions.reduce((sum, region) => sum + region.lines.length, 0);
  assert.strictEqual(calls.polyline.length, boundaryLines, 'only borders are lines');
  const recordPoints = day.entries.reduce((sum, entry) => sum + (entry.kind === 'readings'
    ? (entry.precision === 'precise' ? 1 : entry.points.length)
    : entry.kind === 'visit' ? 1 : 0), 0);
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

const named = {
  ...day,
  entries: [
    { kind: 'visit', device: 0,
      area: { label: 'in Lower Downtown, Denver', town: 'Denver', town_km: 2, within: { neighbourhood: 'Lower Downtown', locality: 'Denver' } },
      place: { margin_m: 40,
        at: { name: 'Union Station', layer: 'building', class: 'train_station', inside: 'Wynkoop Plaza', outline: [[[39.75, -105.0], [39.76, -105.0], [39.76, -104.99]]] },
        near: [{ name: 'Pigtrain <Coffee>', category: 'coffee shop', distance_m: 31 }] },
      arrival: time('08:58'), departure: time('12:06'), point: [39.7527, -105.0003, 40],
      segments: [{ url: '/app/transcripts/20261002?stream=s#085500_300', segment: '085500_300' }] },
    { kind: 'visit', device: 0, area: { label: 'near Denver, Colorado', town: 'Denver', town_km: 2.1 },
      place: { margin_m: 25, at: null, near: [{ name: 'Blue Bottle', category: 'coffee shop', distance_m: 12 }, { name: 'Tattered Cover', category: '', distance_m: 20 }] },
      arrival: time('13:00'), departure: null, point: [39.75, -104.99, 10],
      segments: [{ url: '/app/transcripts/20261002?stream=s#130000_300', segment: '130000_300' }] },
  ],
  accuracy: { offered: { version: 'v1', size_bytes: 1.4e9, files: ['venues.slpk', 'containment.slpk'] },
    installed: { version: 'v1', size_bytes: 1.4e9, venues: true, containment: true }, current: true,
    attributions: ['Places from Overture Maps Foundation'], job: { state: 'idle' } },
};

test('a stop named by the download says at and near, and why', async () => {
  const { elements, calls } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': named }, withLeaflet: true });
  const markup = elements['map-view-root'].innerHTML;
  assert.ok(markup.includes('<p class="map-row-title">at Union Station</p>'));
  assert.ok(markup.includes('in Wynkoop Plaza · in Lower Downtown, Denver'));
  assert.ok(markup.includes('near Pigtrain &lt;Coffee&gt;'), 'venue names are escaped');
  assert.ok(markup.includes('your phone placed you within 40 m, and all of that is inside Union Station.'));
  assert.ok(markup.includes('Pigtrain &lt;Coffee&gt; (coffee shop) is 31 m away; your phone placed you within 40 m.'));
  assert.ok(markup.includes('<p class="map-row-title">near Blue Bottle and Tattered Cover</p>'));
  assert.ok(markup.includes('Tattered Cover is 20 m away'), 'no empty category');
  assert.strictEqual(calls.polygon.length, 1, 'the outline it was inside is drawn');
  assert.ok(markup.includes('Overture Maps'), 'the download is credited');
});

test('the accuracy card offers, follows and removes the download', async () => {
  const card = (accuracy) => {
    const context = { ...day, accuracy };
    return run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': context } })
      .then(({ elements }) => elements['map-view-root'].innerHTML);
  };
  const offered = { version: 'v1', size_bytes: 1.4e9, files: ['venues.slpk'] };
  const fresh = await card({ offered, installed: null, current: false, attributions: [], job: { state: 'idle' } });
  assert.ok(fresh.includes('data-accuracy="download">download 1.4 GB</button>'));
  assert.ok(!fresh.includes('building, park or campus'), 'no outlines promised without the outline file');
  const running = await card({ offered, installed: null, current: false, attributions: [], job: { state: 'downloading', received: 4.12e8, total: 1.4e9 } });
  assert.ok(running.includes('downloading location accuracy: 412 MB of 1.4 GB'));
  const done = await card({ offered, installed: { version: 'v1', size_bytes: 1.4e9, venues: true, containment: false }, current: true, attributions: [], job: { state: 'idle' } });
  assert.ok(done.includes('data-accuracy="remove">remove location accuracy</button>'));
  const full = await card({ offered: null, installed: null, current: false, attributions: [], job: { state: 'idle' } });
  assert.ok(!full.includes('aria-label="location accuracy"'), 'nothing to offer shows no card');
  const failed = await card({ offered, installed: null, current: false, attributions: [], job: { state: 'failed', reason_code: 'not_enough_space' } });
  assert.ok(failed.includes("there isn&#39;t enough free space"));
});

const stayDay = {
  ...day,
  entries: [
    { kind: 'stay', device: 0, count: 18, first: time('08:30'), last: time('09:01'), point: [39.75, -105.0, 30],
      area: { label: 'near Denver, Colorado', town: 'Denver', town_km: 2 },
      place: { margin_m: 30, at: null, near: [{ name: 'Pigtrain Coffee', category: 'coffee shop', distance_m: 12 }] },
      place_ref: { id: '0123456789ab', owner_name: 'the reading room', hidden: false },
      segments: [{ url: '/app/transcripts/20261002?stream=s#083000_300', segment: '083000_300' }] },
    { kind: 'visit', device: 0, area: { label: 'near Denver, Colorado' }, arrival: time('18:00'), departure: null,
      point: [39.74, -104.99, 20], place_ref: { id: 'abcdef012345', owner_name: null, hidden: true }, on_map: false,
      segments: [{ url: '/app/transcripts/20261002?stream=s#180000_300', segment: '180000_300' }] },
    { kind: 'readings', device: 0, area: { label: 'near Denver, Colorado' }, precision: 'approximate',
      first: time('19:00'), last: time('19:30'), count: 3,
      points: [[39.74, -104.99, 900], [39.741, -104.99, 900], [39.80, -104.95, 900]],
      map_points: [[39.80, -104.95, 900]],
      segments: [{ url: '/app/transcripts/20261002?stream=s#190000_300', segment: '190000_300' }] },
  ],
};

test('a worked-out stay says so, and a hidden place stays off the map', async () => {
  const { elements, calls } = await run({ pathname: '/app/map/20261002', responses: { '/app/map/api/day/20261002': stayDay }, withLeaflet: true });
  const markup = elements['map-view-root'].innerHTML;
  assert.ok(markup.includes('<p class="map-row-title">at the reading room</p>'), 'the owner name leads');
  assert.ok(markup.includes('a stay your journal worked out from readings from your iphone · 18 readings'));
  assert.ok(markup.includes('href="/app/map/place/0123456789ab"'));
  assert.ok(markup.includes('hidden from the map'));
  assert.ok(markup.includes('href="/app/map/week/20261002"'), 'the day links to its week');
  const townPoints = day.map.towns.length;
  assert.strictEqual(calls.circleMarker.length, 1 + 1 + townPoints, 'nothing near the hidden place is drawn');
  assert.ok(markup.includes('3 readings'), 'the list still counts every reading');
});

test('the week, places and a place page render from their own data', async () => {
  const week = { start: '20260928', previous: '20260921', next: '20261005', days: [
    { day: '20260928', readings: 0, devices: [], items: [] },
    { day: '20260929', readings: 3, devices: ['iphone'], items: [stayDay.entries[0]] },
  ] };
  const weekPage = await run({ pathname: '/app/map/week/20260929', responses: { '/app/map/api/week/20260929': week } });
  const weekMarkup = weekPage.elements['map-view-root'].innerHTML;
  assert.ok(weekMarkup.includes('the week of Monday, September 28'));
  assert.ok(weekMarkup.includes('no location kept'));
  assert.ok(weekMarkup.includes('data-kind="stay"'));
  assert.ok('data-map-page' in weekPage.elements['map-shell'].attributes, 'the day navigator is hidden');
  const place = { id: '0123456789ab', label: 'the reading room', owner_name: 'the reading room', name: 'the reading room',
    alias: 'home', hidden: false, area: { label: 'near Denver, Colorado' }, words: null, dates: 9, first: '20260929',
    last: '20261009', night_dates: 9, stays: [{ day: '20261009', start: '21:00', end: '07:00', kind: 'reported' }] };
  const places = await run({ pathname: '/app/map/places', responses: { '/app/map/api/places': { places: [place] } } });
  const placesMarkup = places.elements['map-view-root'].innerHTML;
  assert.ok(placesMarkup.includes('visits or stays here on 9 dates, Sep 29 to Oct 9.'));
  assert.ok(placesMarkup.includes('on 9 nights, a visit or stay here covers at least two hours between 22:00 and 06:00.'));
  const empty = await run({ pathname: '/app/map/places', responses: { '/app/map/api/places': { places: [] } } });
  assert.ok(empty.elements['map-view-root'].innerHTML.includes('no places yet.'));
  const page = await run({ pathname: '/app/map/place/0123456789ab', responses: { '/app/map/api/place/0123456789ab': place } });
  const pageMarkup = page.elements['map-view-root'].innerHTML;
  assert.ok(pageMarkup.includes('<option value="home" selected>home</option>'));
  assert.ok(pageMarkup.includes('hide from the map'));
  assert.ok(pageMarkup.includes('21:00–07:00 · a visit your phone reported'));
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
