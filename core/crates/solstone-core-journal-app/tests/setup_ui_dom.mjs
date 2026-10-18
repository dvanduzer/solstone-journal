// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// A click that posts flow does not await the reply. The browser keeps the
// page up when that reply is a refusal; Node would stop the process.
process.on('unhandledRejection', () => {});

const here = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(here, '../ui/index.html'), 'utf8');

class DomNode {
  constructor() {
    this.parentNode = null;
    this.childNodes = [];
    this.nodeType = 1;
  }

  append(...children) {
    for (let child of children) {
      if (child == null || child === false) continue;
      if (typeof child === 'string' || typeof child === 'number') {
        child = document.createTextNode(String(child));
      }
      if (child.parentNode) {
        child.parentNode.childNodes = child.parentNode.childNodes.filter(node => node !== child);
      }
      child.parentNode = this;
      this.childNodes.push(child);
    }
  }

  appendChild(child) {
    this.append(child);
    return child;
  }

  replaceChildren(...children) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    this.append(...children);
    if (document.activeElement && !document.body?.contains(document.activeElement)) {
      document.activeElement = document.body;
    }
  }

  remove() {
    if (!this.parentNode) return;
    this.parentNode.childNodes = this.parentNode.childNodes.filter(node => node !== this);
    this.parentNode = null;
    if (document.activeElement && !document.body?.contains(document.activeElement)) {
      document.activeElement = document.body;
    }
  }

  contains(node) {
    let current = node;
    while (current) {
      if (current === this) return true;
      current = current.parentNode;
    }
    return false;
  }
}

class DomText extends DomNode {
  constructor(data) {
    super();
    this.nodeType = 3;
    this.data = String(data);
    this.tagName = '#text';
    this.localName = '#text';
  }

  get textContent() {
    return this.data;
  }

  set textContent(value) {
    this.data = String(value);
  }
}

class DomElement extends DomNode {
  constructor(tag) {
    super();
    this.tagName = String(tag).toUpperCase();
    this.localName = String(tag).toLowerCase();
    this.attributes = {};
    this.listeners = {};
    this.style = {};
    this.hidden = false;
    this._className = '';
    this._value = '';
    this._disabled = false;
    this.checked = false;
    this.id = '';
    const self = this;
    this.classList = {
      add(...names) {
        const set = new Set(self.className.split(/\s+/).filter(Boolean));
        for (const name of names) set.add(name);
        self.className = [...set].join(' ');
      },
      remove(...names) {
        const drop = new Set(names);
        self.className = self.className.split(/\s+/).filter(name => name && !drop.has(name)).join(' ');
      },
      toggle(name, force) {
        const has = this.contains(name);
        const on = force === undefined ? !has : Boolean(force);
        if (on) this.add(name);
        else this.remove(name);
        return on;
      },
      contains(name) {
        return self.className.split(/\s+/).includes(name);
      },
    };
  }

  get className() {
    return this._className;
  }

  set className(value) {
    this._className = value == null ? '' : String(value);
  }

  get value() {
    return this._value;
  }

  set value(value) {
    this._value = String(value);
  }

  get disabled() {
    return this._disabled;
  }

  set disabled(value) {
    this._disabled = Boolean(value);
  }

  set tabIndex(value) {
    this._tabIndex = value;
    this.attributes.tabindex = String(value);
  }

  get textContent() {
    return this.childNodes.map(child => child.textContent).join('');
  }

  set textContent(value) {
    this.childNodes = [];
    const text = new DomText(value ?? '');
    text.parentNode = this;
    this.childNodes = [text];
  }

  get children() {
    return this.childNodes.filter(child => child.nodeType === 1);
  }

  setAttribute(name, value) {
    this.attributes[name] = String(value);
    if (name === 'id') this.id = String(value);
    if (name === 'class') this.className = String(value);
    if (name === 'value') this._value = String(value);
    if (name === 'disabled') this._disabled = true;
    if (name === 'checked') this.checked = true;
  }

  getAttribute(name) {
    return Object.prototype.hasOwnProperty.call(this.attributes, name) ? this.attributes[name] : null;
  }

  hasAttribute(name) {
    return Object.prototype.hasOwnProperty.call(this.attributes, name);
  }

  addEventListener(type, handler) {
    (this.listeners[type] ||= []).push(handler);
  }

  focus() {
    document.activeElement = this;
  }

  select() {}

  click() {
    const event = { type: 'click', currentTarget: this, target: this };
    for (const handler of this.listeners.click || []) handler(event);
  }

  querySelector(selector) {
    return query(this, selector);
  }

  querySelectorAll(selector) {
    const results = [];
    walk(this, element => { if (matches(element, selector)) results.push(element); });
    return results;
  }
}

function walk(node, visit) {
  for (const child of node.childNodes) {
    visit(child);
    if (child.childNodes) walk(child, visit);
  }
}

function matches(element, selector) {
  if (!(element instanceof DomElement)) return false;
  if (selector.startsWith('.')) return element.classList.contains(selector.slice(1));
  if (selector.startsWith('#')) return element.id === selector.slice(1);
  return element.localName === selector.toLowerCase();
}

function query(root, selector) {
  let current = [root];
  for (const part of selector.trim().split(/\s+/)) {
    const next = [];
    for (const node of current) walk(node, element => { if (matches(element, part)) next.push(element); });
    current = next;
  }
    return current[0] || null;
}

const document = {
  activeElement: null,
  body: null,
  fonts: {
    load: () => Promise.resolve([]),
  },
  createElement(tag) {
    if (tag.toLowerCase() === 'canvas') {
      const el = new DomElement('canvas');
      const ctx = {
        fillStyle: '',
        font: '',
        textAlign: '',
        textBaseline: '',
        globalAlpha: 1,
        strokeStyle: '',
        lineWidth: 1,
        lineCap: '',
        lineJoin: '',
        save() {},
        restore() {},
        setTransform() {},
        fill() {},
        stroke() {},
        translate() {},
        rotate() {},
        scale() {},
        beginPath() {},
        roundRect() {},
        setLineDash() {},
        fillText() {},
        measureText() { return { width: 1 }; },
      };
      el.getContext = () => ctx;
      el.toBlob = (callback) => {
        callback({
          arrayBuffer: async () => new Uint8Array([1]).buffer,
        });
      };
      return el;
    }
    return new DomElement(tag);
  },
  createElementNS(_namespace, tag) {
    return new DomElement(tag);
  },
  createTextNode(text) {
    return new DomText(text);
  },
  getElementById(id) {
    let found = null;
    walk(document.body, element => { if (element.id === id) found = element; });
    return found;
  },
  execCommand() {
    return false;
  },
  querySelector(selector) {
    return query(document.body, selector);
  },
  querySelectorAll(selector) {
    return document.body.querySelectorAll(selector);
  },
};
document.body = new DomElement('body');
document.activeElement = document.body;

globalThis.Node = DomNode;
globalThis.Path2D = class {
  constructor(_arg) {}
  addPath() {}
  arc() {}
  ellipse() {}
  roundRect() {}
  moveTo() {}
  lineTo() {}
  closePath() {}
};

globalThis.DOMParser = class {
  parseFromString(xmlString) {
    const doc = new DomElement('doc');
    const root = new DomElement('svg');
    doc.documentElement = root;
    doc.querySelector = () => null;
    root.querySelector = () => null;

    let inner = String(xmlString).trim();
    const openMatch = inner.match(/^<svg[^>]*>/i);
    if (openMatch) {
      inner = inner.slice(openMatch[0].length);
    }
    const closeMatch = inner.match(/<\/svg>$/i);
    if (closeMatch) {
      inner = inner.slice(0, inner.length - closeMatch[0].length);
    }
    inner = inner.trim();

    const tagRegex = /<([a-zA-Z0-9]+)\s*([^>]*?)(?:\/>|>[\s\S]*?<\/\1>|>)/g;
    let match;
    while ((match = tagRegex.exec(inner)) !== null) {
      const tagName = match[1].toLowerCase();
      const rawAttrs = match[2];
      const el = new DomElement(tagName);
      el.localName = tagName;
      const attrRegex = /([a-zA-Z0-9_-]+)=["']([^"']*)["']/g;
      let attrMatch;
      while ((attrMatch = attrRegex.exec(rawAttrs)) !== null) {
        el.setAttribute(attrMatch[1], attrMatch[2]);
      }
      root.append(el);
    }
    return doc;
  }
};
globalThis.document = document;
globalThis.window = globalThis;

const realSetInterval = globalThis.setInterval.bind(globalThis);
globalThis.setInterval = (fn, ms, ...args) => {
  const timer = realSetInterval(fn, ms, ...args);
  timer.unref?.();
  return timer;
};

const ids = ['loading', 'firstrun', 'main', 'panes', 'pane', 'admin-terminal', 'quit', 'overlay', 'overlay-text'];
for (const id of ids) {
  if (!html.includes(`id="${id}"`)) throw new Error(`index.html is missing #${id}`);
  const tag = id === 'pane' ? 'main' : id === 'panes' ? 'ul' : id === 'firstrun' ? 'section' : 'div';
  const element = document.createElement(tag);
  element.id = id;
  if (new RegExp(`id="${id}"[^>]*\\shidden`).test(html) || html.includes(`id="${id}" hidden`)) element.hidden = true;
  document.body.append(element);
}

const calls = [];
const inbox = [];
const statusPlan = [];
const startPlan = [];
const syncPlan = [];
const identityPlan = [];
const flowPlan = [];
const setIconCalls = [];

function send(id, ok, extra) {
  window.journalApp.receive({ type: 'reply', id, ok, ...extra });
}

function cachedSeedMark() {
  return {
    words: ['cached', 'seed'],
    icon1: {
      svg: '<path d="M 2 2 L 20 20" />',
      color: { name: 'orange', hex: '#E8913A' },
      rot: 0,
    },
    icon2: {
      svg: '<circle cx="12" cy="12" r="10" />',
      color: { name: 'gold', hex: '#D4A017' },
      rot: 45,
    },
  };
}

function statusBody(display, installed = false) {
  return {
    service: {
      installed,
      port: 5016,
      starts_at_sign_in: true,
      journal: 'C:\\journal',
      running: display === 'running',
    },
    set_up: false,
    answering: display === 'running',
    display,
    about: 'about',
    base: 'http://127.0.0.1:5016',
  };
}

function view(extra) {
  return {
    busy: false,
    error: null,
    resting: false,
    sign_in: true,
    step: 1,
    watch_downloads: false,
    local_offered: false,
    thinking_saved: false,
    thinking_choice: null,
    thinking_cancel: false,
    thinking_retry: false,
    speech_retry: false,
    can_finalize: false,
    setup_running: false,
    setup_failed: false,
    mark: { proposal_id: null, render: null, state: 'unavailable' },
    downloads: {},
    models: { showing: false, detail: null, pending: false },
    copy: {
      heading: 'Head',
      welcome: { title: 'Welcome', body: 'Body' },
      fits: {
        title: 'Fits',
        journal_term: 'Journal',
        journal: 'keeps',
        app_term: 'App',
        app: 'takes',
        thinking_term: 'Think',
        thinking: 'helps',
      },
      login: { label: 'Sign in', help: 'Help' },
      folder: { title: 'Folder', help: 'Where' },
      mark: { title: 'Mark', help: 'Mh', error: 'mark-err', permanence: 'perm', try_another: 'Try', continue: 'Keep' },
      thinking: {
        title: 'Thinking',
        change: 'Change',
        later: 'Later',
        choose_later: 'Choose later',
        continue: 'Continue',
        choices: [
          { id: 'local', label: 'Local lane', sub: 'on this pc', description: 'local description' },
          { id: 'spp', label: 'Spp lane', sub: 'spp', description: 'spp description' },
          { id: 'byo', label: 'Byo lane', sub: 'byo', description: 'byo description' },
        ],
      },
      downloads: {
        speech: 'Speech',
        thinking: 'ThinkDl',
        retry: 'Retry',
        cancel: 'Cancel',
        cancelled: 'Cancelled',
      },
    },
    ...extra,
  };
}

window.qrcode = () => ({
  addData() {},
  make() {},
  getModuleCount: () => 2,
  isDark: () => false,
});

let homeHasLanded = false;

window.chrome = {
  webview: {
    postMessage(raw) {
      const message = JSON.parse(raw);
      const { id, cmd, args } = message;
      calls.push({ cmd, args });
      if (cmd === 'init') {
        send(id, true, {
          value: {
            launch: 'sign-in',
            app_version: '0',
            about: 'about',
            default_location: 'C:\\journal',
            mark: cachedSeedMark(),
            flow: null,
          },
        });
        return;
      }
      if (cmd === 'status') {
        const planned = statusPlan.shift();
        if (planned && planned.fail) {
          send(id, false, { error: planned.fail });
        } else if (planned) {
          send(id, true, { value: planned });
        } else {
          send(id, true, { value: statusBody('not-running', false) });
        }
        return;
      }
      if (cmd === 'start') {
        const planned = startPlan.shift();
        if (planned && planned.fail) {
          send(id, false, { error: planned.fail });
        } else if (planned) {
          send(id, true, { value: planned });
        } else {
          send(id, true, { value: statusBody('running', true) });
        }
        return;
      }
      if (cmd === 'syncSignInLaunch') {
        const planned = syncPlan.shift();
        if (planned && planned.fail) {
          send(id, false, { error: planned.fail });
        } else {
          send(id, true, { value: null });
        }
        return;
      }
      if (cmd === 'setIcon') {
        setIconCalls.push(args);
        send(id, true, { value: null });
        return;
      }
      if (cmd === 'ping' || cmd === 'holdsJournal' || cmd === 'quit' || cmd === 'adminTerminal') {
        send(id, true, { value: cmd === 'ping' ? { answering: false } : cmd === 'holdsJournal' ? false : null });
        return;
      }
      if (cmd === 'convey') {
        const path = args?.path || '';
        if (path === '/app/link/api/identity') {
          const planned = identityPlan.shift();
          if (planned && planned.fail) {
            send(id, false, { error: planned.fail });
          } else if (planned) {
            send(id, true, { value: planned });
          } else {
            send(id, true, { value: { committed: false, instance_id: null, mark: null } });
          }
          return;
        }
        let value = {};
        if (path === '/app/network/api/devices') value = { devices: [] };
        else if (path === '/app/network/pair-start') {
          value = { pair_link: 'https://pair.example/link', nonce: 'nonce-1', expires_in: 60 };
        } else if (path === '/app/network/api/local-network') value = { open: true };
        else value = { ok: true };
        send(id, true, { value });
        return;
      }
      if (cmd === 'pairingStatus') {
        send(id, true, { value: { used: false } });
        return;
      }
      if (cmd === 'flow') {
        if (args?.intent === 'boot') {
          send(id, true, { value: view({ phase: 'welcome', step: 1 }) });
          return;
        }
        if (flowPlan.length > 0) {
          const planned = flowPlan.shift();
          if (planned.manual) {
            inbox.push(message);
            return;
          }
          if (planned.error) {
            send(id, false, { error: planned.error });
            return;
          }
          if (planned.value) {
            if (planned.value?.phase === 'home') homeHasLanded = true;
            send(id, true, { value: planned.value });
            return;
          }
        }
        if (args?.intent === 'retry-state' && homeHasLanded) {
          send(id, true, { value: view({ phase: 'home', step: 5 }) });
          return;
        }
        inbox.push(message);
        return;
      }
      inbox.push(message);
    },
  },
};

function flush() {
  return new Promise(resolve => setImmediate(resolve));
}

function $(id) {
  return document.getElementById(id);
}

function visible(element) {
  let current = element;
  while (current) {
    if (current.hidden) return false;
    current = current.parentNode;
  }
  return true;
}

function buttons() {
  const found = [];
  walk(document.body, element => {
    if (element.tagName === 'BUTTON' && visible(element)) found.push(element);
  });
  return found;
}

function click(label) {
  const button = buttons().find(button => button.textContent.trim() === label);
  if (!button) throw new Error(`no visible button ${label}; saw ${buttons().map(button => button.textContent.trim()).join(' | ')}`);
  button.click();
}

function answer(value) {
  const message = inbox.shift();
  if (!message) throw new Error('no pending call');
  send(message.id, true, { value });
}

function fail(error) {
  const message = inbox.shift();
  if (!message) throw new Error('no pending call');
  send(message.id, false, { error });
}

function paint(value) {
  if (value?.phase === 'home') homeHasLanded = true;
  window.journalApp.receive({ type: 'flow', view: value });
}

let passed = 0;
function pass(name) {
  passed += 1;
  if (process.env.DOM_CASE_LOG) console.error('ok', name);
}

const root = () => $('firstrun');

statusPlan.push({ fail: "couldn't read journal status" });

await import('../ui/app.js');
await flush();

if (!visible($('loading'))) throw new Error('#loading is not visible on initial status failure');
if (visible(root())) throw new Error('welcome is painted on initial status failure');
click('try again');
await flush();
if (calls.some(c => c.cmd === 'flow' && c.args?.intent === 'start-setup') || calls.some(c => c.cmd === 'open')) {
  throw new Error('initial status failure called start-setup or open');
}
pass('initial status failure then retry');

if (root().getAttribute('data-phase') !== 'welcome') {
  throw new Error(`boot did not paint welcome (${root().getAttribute('data-phase')}) ${root().textContent}`);
}

click('continue');
answer(view({ phase: 'sign-in', step: 1 }));
if (root().getAttribute('data-phase') !== 'sign-in') throw new Error('welcome continue did not paint sign-in');
if (document.activeElement?.tagName !== 'H1') throw new Error('sign-in did not focus its heading');
click('continue');
answer(view({ phase: 'folder', step: 2 }));
if (root().getAttribute('data-phase') !== 'folder') throw new Error('sign-in continue did not paint folder');
pass('welcome and sign-in continue');

const folderInput = buttons().length >= 0 && [...root().childNodes].length
  ? query(root(), 'input')
  : null;
if (!folderInput || folderInput.tagName !== 'INPUT') throw new Error('folder input is missing');
folderInput.focus();
folderInput.value = 'typed-path';
paint(view({ phase: 'folder', step: 2 }));
if (document.activeElement !== folderInput) throw new Error('folder refresh moved focus');
if (folderInput.value !== 'typed-path') throw new Error('folder refresh cleared the typed value');
pass('heading focus and folder typing');

click('continue');
answer(view({ phase: 'setup', busy: true, error: 'busy-err' }));
if (!root().textContent.includes('busy-err')) throw new Error('busy reply hid the error');
if (!buttons().some(button => button.disabled)) throw new Error('busy reply left every button enabled');
pass('busy reply shows the error');

click('try again');
fail('visible-err');
if (!root().textContent.includes('visible-err')) throw new Error('rejected reply hid the error');
pass('rejected reply is visible');

click('try again');
const phases = [];
answer(view({ phase: 'mark', step: 3, busy: true, error: 'mark-busy' }));
phases.push(root().getAttribute('data-phase'));
paint(view({ phase: 'thinking', step: 4 }));
phases.push(root().getAttribute('data-phase'));
if (phases[0] !== 'mark' || phases[1] !== 'thinking') {
  throw new Error(`reply did not paint before the later flow (${phases.join(' -> ')})`);
}
pass('reply paints before a later flow');

paint(view({
  phase: 'thinking',
  step: 4,
  thinking_saved: true,
  thinking_choice: 'local',
  local_offered: true,
}));
if (!root().textContent.includes('Local lane')) throw new Error('saved choice hid its label');
if (buttons().some(button => button.textContent.trim() === 'Later')) throw new Error('saved choice already listed Later');
click('Change');
if (!buttons().some(button => button.textContent.trim() === 'Later')) throw new Error('Change did not reveal Later');
click('Later');
const later = inbox.shift();
if (later?.args?.choice !== 'later') throw new Error(`Later posted ${JSON.stringify(later?.args)}`);
pass('saved choice can change to later');

const downloading = view({
  phase: 'thinking',
  step: 4,
  thinking_cancel: true,
  downloads: {
    thinking: { state: 'downloading', attempt_id: 'painted-a', bytes_done: 12, bytes_total: 40 },
  },
});
paint(downloading);
if (!root().textContent.includes('ThinkDl') || !root().textContent.includes('12') || !root().textContent.includes('40')) {
  throw new Error(`download card hid its label or bytes (${root().textContent})`);
}
downloading.downloads.thinking.attempt_id = 'newer-b';
click('Cancel');
const cancel = inbox.shift();
if (cancel?.args?.attempt_id !== 'painted-a') throw new Error(`cancel posted ${cancel?.args?.attempt_id}`);
pass('cancel posts the painted attempt');

paint(view({ phase: 'home', step: 5, error: 'home-err' }));
if ($('main').hidden) throw new Error('home left the main window hidden');
if (!$('pane').textContent.includes('home-err')) throw new Error('home hid the error');
pass('home shows its error');

click('start');
await flush();
click('devices');
await flush();
click('pair a device');
await flush();
const pairing = $('pane').querySelector('.pairing');
if (!pairing) throw new Error('pairing dialog did not open');
const devices = buttons().find(button => button.textContent.trim() === 'devices');
if (devices?.getAttribute('aria-selected') !== 'true') throw new Error('devices pane was not selected');
paint(view({ phase: 'home', step: 5 }));
if ($('pane').querySelector('.pairing') !== pairing) throw new Error('a later home update replaced the pairing dialog');
if (devices.getAttribute('aria-selected') !== 'true') throw new Error('a later home update left the devices page');
pass('later home leaves the open page alone');

click('close');
await flush();
click('home');
await flush();

// 1. unsaved capable thinking shows the local line and continue saves local
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_capability: 'ok',
  local_offered: true,
  default_line: 'measured-line',
  thinking_saved: false,
  thinking_choice: null,
}));
if (!root().textContent.includes('measured-line')) throw new Error('measured-line not visible');
if (!buttons().some(b => b.textContent.trim() === 'Change')) throw new Error('Change button not visible');
if (!buttons().some(b => b.textContent.trim() === 'Later')) throw new Error('Later button not visible');
if (!buttons().some(b => b.textContent.trim() === 'Continue')) throw new Error('Continue button not visible');
if (inbox.length !== 0) throw new Error(`inbox not empty: ${JSON.stringify(inbox)}`);
click('Continue');
const chosen = inbox.shift();
if (chosen?.cmd !== 'flow' || chosen?.args?.intent !== 'continue-thinking' || chosen?.args?.choice !== 'local') {
  throw new Error(`expected continue-thinking local, got: ${JSON.stringify(chosen)}`);
}
pass('unsaved capable thinking shows the local line and continue saves local');

// 2. change reveals the three lanes
click('Change');
const changeText = root().textContent;
for (const choice of ['Local lane', 'on this pc', 'local description', 'Spp lane', 'spp', 'spp description', 'Byo lane', 'byo', 'byo description']) {
  if (!changeText.includes(choice)) throw new Error(`missing in change: ${choice}`);
}
pass('change reveals the three lanes');

// 3. unsaved blocked shows choose later
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_capability: 'blocked',
  local_offered: false,
  default_line: 'measured-line',
  thinking_saved: false,
  thinking_choice: null,
}));
if (!root().textContent.includes('Choose later')) throw new Error('choose_later text not visible');
if (root().textContent.includes('measured-line')) throw new Error('default_line visible');
if (buttons().some(b => b.textContent.trim() === 'Continue')) throw new Error('Continue button visible');
if (!buttons().some(b => b.textContent.trim() === 'Spp lane')) throw new Error('Spp lane not visible');
if (!buttons().some(b => b.textContent.trim() === 'Byo lane')) throw new Error('Byo lane not visible');
if (!buttons().some(b => b.textContent.trim() === 'Later')) throw new Error('Later not visible');
click('Later');
const laterMsg = inbox.shift();
if (laterMsg?.cmd !== 'flow' || laterMsg?.args?.intent !== 'continue-thinking' || laterMsg?.args?.choice !== 'later') {
  throw new Error(`expected continue-thinking later, got: ${JSON.stringify(laterMsg)}`);
}
pass('unsaved blocked shows choose later');

// 4. unsaved unknown shows choose later
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_capability: 'unknown',
  local_offered: false,
  default_line: 'measured-line',
  thinking_saved: false,
  thinking_choice: null,
}));
if (!root().textContent.includes('Choose later')) throw new Error('choose_later text not visible');
if (root().textContent.includes('measured-line')) throw new Error('default_line visible');
if (buttons().some(b => b.textContent.trim() === 'Continue')) throw new Error('Continue button visible');
if (!buttons().some(b => b.textContent.trim() === 'Spp lane')) throw new Error('Spp lane not visible');
if (!buttons().some(b => b.textContent.trim() === 'Byo lane')) throw new Error('Byo lane not visible');
if (!buttons().some(b => b.textContent.trim() === 'Later')) throw new Error('Later not visible');
pass('unsaved unknown shows choose later');

// 5. saved local spp byo and later name themselves
const savedCases = [
  { id: 'local', label: 'Local lane' },
  { id: 'spp', label: 'Spp lane' },
  { id: 'byo', label: 'Byo lane' },
  { id: 'later', label: 'Later' },
];
for (const sc of savedCases) {
  paint(view({
    phase: 'thinking',
    step: 4,
    thinking_saved: true,
    thinking_choice: sc.id,
  }));
  if (!root().textContent.includes(sc.label)) throw new Error(`${sc.id} label not visible`);
  if (sc.id !== 'later' && buttons().some(b => b.textContent.trim() === 'Later')) {
    throw new Error(`${sc.id} showed Later button before Change`);
  }
  click('Change');
  if (!buttons().some(b => b.textContent.trim() === 'Later')) {
    throw new Error(`${sc.id} Change did not reveal Later`);
  }
}
pass('saved local spp byo and later name themselves');

// 6. proposed null does not commit
paint(view({
  phase: 'mark',
  step: 3,
  mark: { proposal_id: 'prop-1', render: null, state: 'proposed' },
}));
click('Keep');
if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error('null render committed');
pass('proposed null does not commit');

// 7. empty mark does not commit
paint(view({
  phase: 'mark',
  step: 3,
  mark: { proposal_id: 'prop-1', render: {}, state: 'proposed' },
}));
click('Keep');
if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error('empty mark committed');
pass('empty mark does not commit');

// 8. malformed mark does not commit
const badMarks = [
  { words: ['only-one'], icon1: { svg: '<path d="M1 1L5 5"/>', color: { name: 'c', hex: '#112233' }, rot: 0 }, icon2: { svg: '<circle cx="1" cy="1" r="1"/>', color: { name: 'c', hex: '#445566' }, rot: 0 } },
  { words: ['bad', 'hex'], icon1: { svg: '<path d="M1 1L5 5"/>', color: { name: 'c', hex: 'not-hex' }, rot: 0 }, icon2: { svg: '<circle cx="1" cy="1" r="1"/>', color: { name: 'c', hex: '#445566' }, rot: 0 } },
  { words: ['empty', 'color'], icon1: { svg: '<path d="M1 1L5 5"/>', color: { name: '', hex: '#112233' }, rot: 0 }, icon2: { svg: '<circle cx="1" cy="1" r="1"/>', color: { name: 'c', hex: '#445566' }, rot: 0 } },
];
for (const bm of badMarks) {
  paint(view({
    phase: 'mark',
    step: 3,
    mark: { proposal_id: 'prop-1', render: bm, state: 'proposed' },
  }));
  click('Keep');
  if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error('bad mark committed');
}
pass('malformed mark does not commit');

// 9. literal mark shows both glyphs and both words and commits the painted id
const validRender = {
  words: ['oak', 'river'],
  icon1: { svg: '<path d="M1 1L5 5"/>', color: { name: 'c1', hex: '#112233' }, rot: 0 },
  icon2: { svg: '<circle cx="1" cy="1" r="1"/>', color: { name: 'c2', hex: '#445566' }, rot: 45 },
};
paint(view({
  phase: 'mark',
  step: 3,
  mark: { proposal_id: 'prop-painted-id', render: validRender, state: 'proposed' },
}));
if (!root().textContent.includes('oak') || !root().textContent.includes('river')) {
  throw new Error('words oak and river not visible');
}
if (!query(root(), 'path') || !query(root(), 'circle')) {
  throw new Error('path and circle not in card');
}
click('Keep');
const commitMsg = inbox.shift();
if (commitMsg?.cmd !== 'flow' || commitMsg?.args?.intent !== 'commit-mark' || commitMsg?.args?.proposal_id !== 'prop-painted-id') {
  throw new Error(`commit-mark did not post painted id: ${JSON.stringify(commitMsg)}`);
}
pass('literal mark shows both glyphs and both words and commits the painted id');

// 10. mark whose glyphs strip does not commit
const stripRender = {
  words: ['oak', 'river'],
  icon1: { svg: '<g></g>', color: { name: 'c1', hex: '#112233' }, rot: 0 },
  icon2: { svg: '<script></script>', color: { name: 'c2', hex: '#445566' }, rot: 45 },
};
paint(view({
  phase: 'mark',
  step: 3,
  mark: { proposal_id: 'prop-strip', render: stripRender, state: 'proposed' },
}));
click('Keep');
if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error('stripped mark committed');
pass('mark whose glyphs strip does not commit');
for (const svg of ['<path/>', '<path d=""/>', '<path d="M0 0"/>', '<circle r="0"/>']) {
  paint(view({ phase: 'mark', step: 3,
    mark: { proposal_id: 'empty-glyph', state: 'proposed', render: {
      ...validRender, icon1: { ...validRender.icon1, svg },
    } },
  }));
  click('Keep');
  if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error(`empty geometry committed: ${svg}`);
}
pass('empty geometry never enables commit');
const catalog = JSON.parse(readFileSync(join(here, '../../solstone-core-sol-link/assets/mark_assets/glyphs.json'), 'utf8'));
const markModule = await import('../ui/mark.js');
if (!catalog.drum) throw new Error('catalog control missing drum');
for (const [name, svg] of Object.entries(catalog)) {
  const render = { ...validRender,
    icon1: { ...validRender.icon1, svg }, icon2: { ...validRender.icon2, svg },
  };
  if (!markModule.displayableMark(render)) throw new Error(`catalog glyph rejected: ${name}`);
  const card = document.createElement('div');
  markModule.renderMarkCard(card, render);
  const expected = new DOMParser().parseFromString(`<svg>${svg}</svg>`, 'image/svg+xml').documentElement.children;
  for (const tag of ['path', 'circle', 'rect', 'line', 'polyline', 'polygon', 'ellipse']) {
    const originals = [...expected].filter(shape => shape.localName === tag);
    const copies = [...card.querySelectorAll(tag)];
    if (copies.length !== originals.length * 2) throw new Error(`catalog shapes changed: ${name}/${tag}`);
    if (tag === 'path' && copies.some((shape, index) => shape.getAttribute('d') !== originals[index % originals.length].getAttribute('d'))) {
      throw new Error(`catalog path changed: ${name}`);
    }
  }
}
pass('unchanged canonical catalog keeps every drawing shape and path');

paint(view({ phase: 'mark', step: 3, busy: true,
  mark: { proposal_id: null, state: 'unavailable', render: null },
}));
if (!query(root(), 'progress')) throw new Error('first mark creation has no loading indicator');
if (root().textContent.includes('mark-err')) throw new Error('first mark creation shows the expected absence as an error');
pass('first mark creation stays loading');


// 11. committed identity sets the icon
const identityMark = {
  words: ['mountain', 'sky'],
  icon1: { svg: '<path d="M 1 1 L 8 8"/>', color: { name: 'blue', hex: '#112233' }, rot: 0 },
  icon2: { svg: '<circle cx="1" cy="1" r="1"/>', color: { name: 'teal', hex: '#445566' }, rot: 45 },
};
identityPlan.push({
  committed: true,
  instance_id: 'inst-1',
  mark: identityMark,
});
statusPlan.push(statusBody('running', true));
paint(view({ phase: 'home', step: 5 }));
window.journalApp.receive({ type: 'shown' });
await flush();
if (!setIconCalls.some(c => JSON.stringify(c.mark) === JSON.stringify(identityMark))) {
  throw new Error('setIcon not called with identityMark');
}
if (!$('pane').textContent.includes('mountain') || !$('pane').textContent.includes('sky')) {
  throw new Error('identity words not in card');
}
if (!query($('pane'), '.mark-confirmed')) {
  throw new Error('.mark-confirmed node missing');
}
pass('committed identity sets the icon');

// 12. neutral identity does not confirm the cached mark
const setIconCountBefore = setIconCalls.length;
identityPlan.push({ committed: false, instance_id: null, mark: null });
statusPlan.push(statusBody('running', true));
window.journalApp.receive({ type: 'shown' });
await flush();
if (setIconCalls.length !== setIconCountBefore) throw new Error('new setIcon called on neutral identity');
if ($('pane').textContent.includes('cached') || $('pane').textContent.includes('seed')) {
  throw new Error('cached words shown on neutral identity');
}
if (query($('pane'), '.mark-confirmed')) throw new Error('.mark-confirmed shown on neutral identity');
pass('neutral identity does not confirm the cached mark');

// 13. malformed identity does not confirm the cached mark
identityPlan.push({ committed: true, instance_id: 'inst-2', mark: null });
statusPlan.push(statusBody('running', true));
window.journalApp.receive({ type: 'shown' });
await flush();
if (setIconCalls.length !== setIconCountBefore) throw new Error('new setIcon called on malformed identity');
if ($('pane').textContent.includes('cached') || $('pane').textContent.includes('seed')) {
  throw new Error('cached words shown on malformed identity');
}
if (query($('pane'), '.mark-confirmed')) throw new Error('.mark-confirmed shown on malformed identity');
pass('malformed identity does not confirm the cached mark');

// 14. failed identity does not confirm the cached mark
identityPlan.push({ fail: 'convey failed' });
statusPlan.push(statusBody('running', true));
window.journalApp.receive({ type: 'shown' });
await flush();
if (setIconCalls.length !== setIconCountBefore) throw new Error('new setIcon called on failed identity');
if ($('pane').textContent.includes('cached') || $('pane').textContent.includes('seed')) {
  throw new Error('cached words shown on failed identity');
}
if (query($('pane'), '.mark-confirmed')) throw new Error('.mark-confirmed shown on failed identity');
pass('failed identity does not confirm the cached mark');

// 15. thinking focus survives a progress repaint
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_saved: true,
  thinking_choice: 'local',
}));
const changeBtn = buttons().find(b => b.textContent.trim() === 'Change');
if (!changeBtn) throw new Error('Change button not found');
changeBtn.focus();
paint(view({
  phase: 'thinking', step: 4, thinking_saved: true, thinking_choice: 'local', busy: true,
  downloads: { thinking: { state: 'downloading', bytes_done: 5, bytes_total: 100 } },
}));
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_saved: true,
  thinking_choice: 'local',
  downloads: { thinking: { state: 'downloading', bytes_done: 10, bytes_total: 100 } },
}));
if (document.activeElement?.textContent.trim() !== 'Change') {
  throw new Error(`focus lost on thinking repaint: active is ${document.activeElement?.tagName}`);
}
pass('thinking focus survives a progress repaint');

// 16. home focus survives a background repaint
paint(view({ phase: 'home', step: 5 }));
statusPlan.push(statusBody('not-running', false));
window.journalApp.receive({ type: 'shown' });
await flush();
const startBtn = buttons().find(b => b.textContent.trim() === 'start');
if (!startBtn) throw new Error('start button not found');
startBtn.focus();
paint(view({ phase: 'home', step: 5 }));
if (document.activeElement?.textContent.trim() !== 'start') {
  throw new Error(`focus lost on home repaint: active is ${document.activeElement?.tagName}`);
}
pass('home focus survives a background repaint');

// 17. a step change focuses the new heading
paint(view({ phase: 'mark', step: 3 }));
if (document.activeElement?.tagName !== 'H1') {
  throw new Error(`phase change did not focus H1, focused: ${document.activeElement?.tagName}`);
}
pass('a step change focuses the new heading');

// 18. a removed thinking control is not focused
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_cancel: true,
  downloads: { thinking: { state: 'downloading', attempt_id: 'att-1', bytes_done: 5, bytes_total: 50 } },
}));
const cancelBtn = buttons().find(b => b.textContent.trim() === 'Cancel');
if (!cancelBtn) throw new Error('Cancel button not found');
cancelBtn.focus();
paint(view({
  phase: 'thinking',
  step: 4,
  thinking_cancel: false,
  downloads: {},
}));
if (document.activeElement === cancelBtn || document.activeElement?.tagName === 'H1') {
  throw new Error(`removed control focused or stole heading: ${document.activeElement?.tagName}`);
}
pass('a removed thinking control is not focused');

// 19. mark thinking and home show retry when authority is clear
flowPlan.push({ manual: true });
paint(view({
  phase: 'mark',
  step: 3,
  authoritative: false,
  mark: { proposal_id: 'prop-auth', render: validRender, state: 'proposed' },
}));
if (!buttons().some(b => b.textContent.trim() === 'try again')) throw new Error('mark missing try again on unauthoritative');
click('Keep');
if (inbox.some(m => m.args?.intent === 'commit-mark')) throw new Error('unauthoritative mark committed');
click('try again');
const retryMarkMsg = inbox.shift();
if (retryMarkMsg?.cmd !== 'flow' || retryMarkMsg?.args?.intent !== 'retry-state') {
  throw new Error(`try again did not post retry-state: ${JSON.stringify(retryMarkMsg)}`);
}

flowPlan.push({ manual: true });
paint(view({
  phase: 'thinking',
  step: 4,
  authoritative: false,
  thinking_saved: false,
  thinking_capability: 'ok',
  local_offered: true,
  default_line: 'line',
}));
if (!buttons().some(b => b.textContent.trim() === 'try again')) throw new Error('thinking missing try again on unauthoritative');
click('Continue');
if (inbox.some(m => ['choose-thinking', 'continue-thinking'].includes(m.args?.intent))) throw new Error('unauthoritative thinking action posted');
click('try again');
const retryThinkMsg = inbox.shift();
if (retryThinkMsg?.cmd !== 'flow' || retryThinkMsg?.args?.intent !== 'retry-state') {
  throw new Error(`try again did not post retry-state: ${JSON.stringify(retryThinkMsg)}`);
}

flowPlan.push({ manual: true });
paint(view({
  phase: 'home',
  step: 5,
  authoritative: false,
}));
if (!buttons().some(b => b.textContent.trim() === 'try again')) throw new Error('home missing try again on unauthoritative');
click('try again');
const retryHomeMsg = inbox.shift();
if (retryHomeMsg?.cmd !== 'flow' || retryHomeMsg?.args?.intent !== 'retry-state') {
  throw new Error(`try again did not post retry-state: ${JSON.stringify(retryHomeMsg)}`);
}
pass('mark thinking and home show retry when authority is clear');

// 20. stopped registration can start
const callsCountBefore20 = calls.length;
statusPlan.push(statusBody('stopped', true));
window.journalApp.receive({ type: 'shown' });
await flush();
paint(view({ phase: 'opening' }));
if ($('main').hidden) throw new Error('main is hidden');
if (!buttons().some(b => b.textContent.trim() === 'start')) throw new Error('start button not visible');
if (root().getAttribute('data-phase') === 'home') throw new Error('phase is home');
const newCalls20 = calls.slice(callsCountBefore20);
if (newCalls20.some(c => c.cmd === 'open' || (c.cmd === 'flow' && c.args?.intent === 'start-setup'))) {
  throw new Error('called open or start-setup');
}
pass('stopped registration can start');

// 21. failed start is visible
startPlan.push({ fail: 'start daemon failed' });
click('start');
await flush();
if (!$('pane').textContent.includes('start daemon failed')) throw new Error('start error not visible in pane');
if (inbox.some(m => m.args?.intent === 'retry-state')) throw new Error('failed start posted retry-state');
pass('failed start is visible');

// 22. started registration reconciles to the incomplete step
flowPlan.push({ manual: true });
startPlan.push(statusBody('running', true));
click('start');
await flush();
const startMsg = inbox.shift();
if (startMsg?.cmd !== 'flow' || startMsg?.args?.intent !== 'retry-state') {
  throw new Error(`expected retry-state after start, got: ${JSON.stringify(startMsg)}`);
}
send(startMsg.id, true, { value: view({ phase: 'mark', step: 3 }) });
await flush();
if (!visible($('firstrun')) || root().getAttribute('data-phase') !== 'mark') {
  throw new Error('firstrun did not show mark');
}
if (calls.some(c => c.cmd === 'open')) throw new Error('open called');
pass('started registration reconciles to the incomplete step');

// 23. started registration reconciles to home
const callsCountBefore23 = calls.length;
statusPlan.push(statusBody('stopped', true));
$('main').hidden = false;
window.journalApp.receive({ type: 'shown' });
await flush();
paint(view({ phase: 'opening' }));
flowPlan.push({ manual: true });
startPlan.push(statusBody('running', true));
click('start');
await flush();
const startHomeMsg = inbox.shift();
if (startHomeMsg?.cmd !== 'flow' || startHomeMsg?.args?.intent !== 'retry-state') {
  throw new Error(`expected retry-state after start, got: ${JSON.stringify(startHomeMsg)}`);
}
send(startHomeMsg.id, true, { value: view({ phase: 'home', step: 5 }) });
await flush();
if ($('main').hidden) throw new Error('main not shown after reconciling to home');
const newCalls23 = calls.slice(callsCountBefore23);
if (newCalls23.some(c => c.cmd === 'open' || (c.cmd === 'flow' && c.args?.intent === 'start-setup'))) {
  throw new Error('called open or start-setup');
}
pass('started registration reconciles to home');

// 24. fresh setup refreshes status before the home action
paint(view({ phase: 'setup', setup_running: true }));
statusPlan.push({
  service: { installed: true, port: 5016, starts_at_sign_in: false, journal: 'C:\\journal', running: true },
  set_up: true,
  answering: true,
  display: 'running',
  about: 'about',
  base: 'http://127.0.0.1:5016',
});
paint(view({ phase: 'home', step: 5 }));
await flush();
const primaryBtn = buttons().find(b => b.classList.contains('primary'));
if (!primaryBtn || primaryBtn.textContent.trim() !== 'open your journal') {
  throw new Error(`primary button is not open your journal: saw ${primaryBtn?.textContent.trim()}`);
}
pass('fresh setup refreshes status before the home action');

// 25. registered sign-in readback is not the first status
const lastSyncCall = [...calls].reverse().find(c => c.cmd === 'syncSignInLaunch');
if (!lastSyncCall || lastSyncCall.args?.on !== false) {
  throw new Error(`expected syncSignInLaunch with on: false, got: ${JSON.stringify(lastSyncCall)}`);
}
pass('registered sign-in readback is not the first status');

// 26. run entry failure is visible and retry only updates the app run entry
syncPlan.push({ fail: 'sync run entry failed' });
paint(view({ phase: 'setup', setup_running: true }));
statusPlan.push({
  service: { installed: true, port: 5016, starts_at_sign_in: true, journal: 'C:\\journal', running: true },
  set_up: true,
  answering: true,
  display: 'running',
  about: 'about',
  base: 'http://127.0.0.1:5016',
});
paint(view({ phase: 'home', step: 5 }));
await flush();
if (!$('pane').textContent.includes('sync run entry failed')) {
  throw new Error('sync failure not visible in pane');
}
const callsBeforeRetry = calls.length;
statusPlan.push(statusBody('running', true));
click('try again');
await flush();
const retryCalls = calls.slice(callsBeforeRetry);
if (retryCalls.length < 2 || retryCalls[0].cmd !== 'status' || retryCalls[1].cmd !== 'syncSignInLaunch') {
  throw new Error(`retry calls expected status then syncSignInLaunch, got: ${JSON.stringify(retryCalls)}`);
}
if (retryCalls.some(c => ['signIn', 'start-setup', 'open'].includes(c.cmd))) {
  throw new Error('prohibited command in retry calls');
}
pass('run entry failure is visible and retry only updates the app run entry');

// 27. status failure after setup retries without setup
const callsBefore27 = calls.length;
paint(view({ phase: 'setup', setup_running: true }));
statusPlan.push({ fail: 'status read failed after setup' });
paint(view({ phase: 'home', step: 5 }));
await flush();
if (!visible($('firstrun')) || !$('firstrun').textContent.includes('status read failed after setup')) {
  throw new Error('error not shown in firstrun on post-setup status failure');
}
if (visible($('main'))) {
  throw new Error('home shown on stale display');
}
statusPlan.push(statusBody('running', true));
click('try again');
await flush();
if ($('main').hidden) throw new Error('main not shown after successful status retry');
const calls27 = calls.slice(callsBefore27);
if (calls27.some(c => c.cmd === 'open' || (c.cmd === 'flow' && c.args?.intent === 'start-setup'))) {
  throw new Error('called open or start-setup');
}
pass('status failure after setup retries without setup');

// 28. continue in solstone app and download offer
if (calls.some(c => c.cmd === 'flow' && c.args?.intent === 'continue-solstone')) {
  throw new Error('found prior continue-solstone call');
}
const homeButtons = document.querySelectorAll('button');
const continueBtn = homeButtons.find(b => b.getAttribute('data-intent') === 'continue-solstone');
if (!continueBtn) throw new Error('continue-solstone button not found');
const callsBeforeContinue = calls.length;
continueBtn.click();
await flush();
const continueCalls = calls.slice(callsBeforeContinue);
const continueCall = continueCalls.find(c => c.cmd === 'flow' && c.args?.intent === 'continue-solstone');
if (!continueCall) {
  throw new Error(`expected continue-solstone call, got: ${JSON.stringify(continueCalls)}`);
}

const callsBeforePaint = calls.length;
paint(view({ phase: 'home', step: 5 }));
await flush();
const paintCalls = calls.slice(callsBeforePaint);
if (paintCalls.some(c => c.cmd === 'open')) {
  throw new Error(`paint triggered open call: ${JSON.stringify(paintCalls)}`);
}

paint(view({ phase: 'home', step: 5, solstone_download: true }));
await flush();
const dlButtons = document.querySelectorAll('button');
const downloadBtn = dlButtons.find(b => b.getAttribute('data-intent') === 'download-solstone');
if (!downloadBtn) throw new Error('download-solstone button not found');
const callsBeforeDl = calls.length;
downloadBtn.click();
await flush();
const dlCalls = calls.slice(callsBeforeDl);
const dlCall = dlCalls.find(c => c.cmd === 'flow' && c.args?.intent === 'download-solstone');
if (!dlCall) {
  throw new Error(`expected download-solstone call, got: ${JSON.stringify(dlCalls)}`);
}
if (dlCall.args?.url != null || dlCall.args?.path != null || dlCall.args?.exe != null) {
  throw new Error(`download call contained prohibited args: ${JSON.stringify(dlCall.args)}`);
}

click('devices');
await flush();
const deviceButtons = document.querySelectorAll('button');
const thisPcBtn = deviceButtons.find(b => b.getAttribute('data-pairing') === 'this-pc');
if (!thisPcBtn) throw new Error('this-pc button not found');
const callsBeforeThisPc = calls.length;
thisPcBtn.click();
await flush();
const thisPcCalls = calls.slice(callsBeforeThisPc);
const pairStartCall = thisPcCalls.find(c => c.cmd === 'convey' && c.args?.path === '/app/network/pair-start');
if (!pairStartCall || pairStartCall.args?.body?.same_machine !== true) {
  throw new Error(`expected convey pair-start with same_machine: true, got: ${JSON.stringify(thisPcCalls)}`);
}
if (thisPcCalls.some(c => c.cmd === 'flow' && c.args?.intent === 'continue-solstone')) {
  throw new Error('this-pc click added continue-solstone call');
}
pass('continue in solstone app and download offer');

console.log(`DOM CASES: ${passed} passed`);
