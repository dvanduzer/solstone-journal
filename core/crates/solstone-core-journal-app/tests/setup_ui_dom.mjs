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
  }

  remove() {
    if (!this.parentNode) return;
    this.parentNode.childNodes = this.parentNode.childNodes.filter(node => node !== this);
    this.parentNode = null;
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
  createElement(tag) {
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
};
document.body = new DomElement('body');
document.activeElement = document.body;

globalThis.Node = DomNode;
globalThis.DOMParser = class {
  parseFromString() {
    const doc = new DomElement('div');
    doc.documentElement = doc;
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

const inbox = [];

function send(id, ok, extra) {
  window.journalApp.receive({ type: 'reply', id, ok, ...extra });
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
        choices: [
          { id: 'local', label: 'Local lane', sub: 'on this pc' },
          { id: 'spp', label: 'Spp lane', sub: 'spp' },
          { id: 'byo', label: 'Byo lane', sub: 'byo' },
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

window.chrome = {
  webview: {
    postMessage(raw) {
      const message = JSON.parse(raw);
      const { id, cmd, args } = message;
      if (cmd === 'init') {
        send(id, true, {
          value: {
            launch: 'sign-in',
            app_version: '0',
            about: 'about',
            default_location: 'C:\\journal',
            mark: null,
            flow: null,
          },
        });
        return;
      }
      if (cmd === 'status') {
        send(id, true, { value: statusBody('not-running', false) });
        return;
      }
      if (cmd === 'start') {
        send(id, true, { value: statusBody('running', true) });
        return;
      }
      if (cmd === 'ping' || cmd === 'holdsJournal' || cmd === 'syncSignInLaunch' || cmd === 'quit' || cmd === 'adminTerminal') {
        send(id, true, { value: cmd === 'ping' ? { answering: false } : cmd === 'holdsJournal' ? false : null });
        return;
      }
      if (cmd === 'convey') {
        const path = args?.path || '';
        let value = {};
        if (path === '/app/network/api/devices') value = { devices: [] };
        else if (path === '/app/network/pair-start') {
          value = { pair_link: 'https://pair.example/link', nonce: 'nonce-1', expires_in: 60 };
        } else if (path === '/app/network/api/local-network') value = { open: true };
        else value = { availability: 'absent' };
        send(id, true, { value });
        return;
      }
      if (cmd === 'pairingStatus') {
        send(id, true, { value: { used: false } });
        return;
      }
      if (cmd === 'flow' && args?.intent === 'boot') {
        send(id, true, { value: view({ phase: 'welcome', step: 1 }) });
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
  window.journalApp.receive({ type: 'flow', view: value });
}

await import('../ui/app.js');
await flush();

let passed = 0;
function pass(name) {
  passed += 1;
  if (process.env.DOM_CASE_LOG) console.error('ok', name);
}

const root = () => $('firstrun');
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

console.log(`DOM CASES: ${passed} passed`);
