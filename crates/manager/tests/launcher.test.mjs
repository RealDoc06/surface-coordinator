import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { setImmediate } from 'node:timers/promises';
import vm from 'node:vm';

const source = readFileSync(new URL('../launcher/launcher.js', import.meta.url), 'utf8');
const html = readFileSync(new URL('../launcher/index.html', import.meta.url), 'utf8');
const available = { configured: true, available: true, version: '0.3.0', notes: 'An update.' };

function eventTarget() {
  const listeners = new Map();
  return {
    addEventListener(name, listener) {
      const registered = listeners.get(name) ?? [];
      registered.push(listener);
      listeners.set(name, registered);
    },
    // Bypass native disabled-button suppression to exercise already queued callbacks.
    fire(name, event = {}) {
      return Promise.all((listeners.get(name) ?? []).map((listener) => listener({
        preventDefault() {},
        ...event,
      })));
    },
  };
}

function launcher() {
  const elements = new Map([...html.matchAll(/<([a-z][\w-]*)\b([^>]*\bid="([^"]+)"[^>]*)>/g)]
    .map(([, tagName, attributes, id]) => [id, {
      ...eventTarget(), tagName, hidden: /\bhidden\b/.test(attributes),
      disabled: false, textContent: '', value: '', dataset: {},
    }]));
  const element = (id) => {
    assert.ok(elements.has(id), `Unknown launcher element: ${id}`);
    return elements.get(id);
  };
  const controls = [...elements.values()].filter(({ tagName }) => ['button', 'input'].includes(tagName));
  const stored = new Map();
  const sessionStorage = {
    getItem: (key) => stored.get(key) ?? null,
    setItem: (key, value) => stored.set(key, String(value)),
  };
  const calls = [];
  const status = {
    version: '0.2.2', remote_url: null, local_running: false, local_client_port: 8080,
    key_sync: { at: null, error: null, imported: 0 },
  };
  const window = {
    ...eventTarget(),
    __TAURI__: { core: {
      Channel: class {},
      invoke(command, args) {
        const call = { command, args };
        calls.push(call);
        if (command === 'status') return Promise.resolve({ ...status });
        return new Promise((resolve, reject) => Object.assign(call, { resolve, reject }));
      },
    } },
  };
  vm.runInNewContext(source, {
    window, URL, sessionStorage,
    document: {
      getElementById: element,
      querySelectorAll(selector) {
        assert.equal(selector, 'button, input');
        return controls;
      },
    },
  }, { filename: 'launcher.js' });
  return {
    element, controls, calls, status, sessionStorage,
    callsFor: (command) => calls.filter((call) => call.command === command),
    fire: (id, name = 'click') => element(id).fire(name),
    pageshow: () => window.fire('pageshow', { persisted: false }),
  };
}

async function offerUpdate(app) {
  await app.pageshow();
  assert.equal(app.callsFor('check_update').length, 1);
  app.callsFor('check_update')[0].resolve(available);
  await setImmediate();
  assert.equal(app.element('update').hidden, false);
}

test('automatic and manual update checks share one in-flight invocation', async () => {
  const app = launcher();
  await app.pageshow();
  const manual = app.fire('update-check');
  assert.equal(app.callsFor('check_update').length, 1);
  assert.equal(app.element('update-check').disabled, true);

  app.callsFor('check_update')[0].resolve(available);
  await manual;
  await setImmediate();
  assert.equal(app.element('update').hidden, false);
  assert.equal(app.element('update-title').dataset.version, available.version);
  assert.equal(app.element('update-check').disabled, false);
});

test('an existing update cannot be installed or dismissed during a new check', async () => {
  const app = launcher();
  await offerUpdate(app);
  const checking = app.fire('update-check');
  for (const id of ['update-check', 'update-install', 'update-later']) {
    assert.equal(app.element(id).disabled, true, id);
  }
  const blocked = [app.fire('update-install'), app.fire('update-later')];
  assert.equal(app.callsFor('install_update').length, 0);
  await Promise.all(blocked);
  assert.equal(app.element('update').hidden, false);
  assert.equal(app.sessionStorage.getItem('manager-update-dismissed'), null);

  app.callsFor('check_update')[1].resolve(available);
  await checking;
  for (const id of ['update-check', 'update-install', 'update-later']) {
    assert.equal(app.element(id).disabled, false, id);
  }
});

test('finishing an update check keeps a concurrent local action disabled', async () => {
  const app = launcher();
  await offerUpdate(app);
  const checking = app.fire('update-check');
  assert.equal(app.element('local-open').disabled, false, 'local access stays available during a check');
  const opening = app.fire('local-open');
  assert.equal(app.callsFor('open_local').length, 1);

  app.callsFor('check_update')[1].resolve(available);
  await checking;
  assert.ok(app.controls.every((control) => control.disabled));
  assert.equal(app.element('local-open').textContent, 'Starting…');
  await app.fire('update-install');
  assert.equal(app.callsFor('install_update').length, 0);

  app.callsFor('open_local')[0].resolve();
  await opening;
  await setImmediate();
  assert.ok(app.controls.every((control) => !control.disabled));
  assert.equal(app.element('local-open').textContent, 'Start local coordinator');
});

test('a successful no-update check removes a previously offered release', async () => {
  const app = launcher();
  await offerUpdate(app);
  const checking = app.fire('update-check');
  app.callsFor('check_update')[1].resolve({ configured: true, available: false });
  await checking;

  assert.equal(app.element('update').hidden, true);
  assert.equal(app.element('update-message').textContent, 'You are up to date.');
  await app.fire('update-install');
  assert.equal(app.callsFor('install_update').length, 0);
});

test('installation blocks other commands and preserves progress and disabled controls', async () => {
  const app = launcher();
  await offerUpdate(app);
  const installing = app.fire('update-install');
  const install = app.callsFor('install_update')[0];
  install.args.onProgress.onmessage('Downloading 40%');
  const callCount = app.calls.length;

  const blocked = [
    app.fire('update-check'), app.fire('update-install'), app.fire('update-later'),
    app.fire('local-open'), app.fire('local-stop'), app.fire('remote', 'submit'),
  ];
  assert.equal(app.calls.length, callCount);
  await Promise.all(blocked);
  await setImmediate();
  assert.equal(app.calls.length, callCount);
  assert.equal(app.element('update-progress').textContent, 'Downloading 40%');
  assert.equal(app.element('update').hidden, false);
  assert.ok(app.controls.every((control) => control.disabled));

  install.resolve();
  await installing;
  assert.ok(app.controls.every((control) => control.disabled), 'remain disabled until restart');
});

test('failed installation refreshes local status and permits another attempt', async () => {
  const app = launcher();
  await offerUpdate(app);
  app.status.local_running = true;
  const statusCalls = app.callsFor('status').length;
  const installing = app.fire('update-install');
  app.callsFor('install_update')[0].reject(new Error('Download interrupted'));
  await installing;
  await setImmediate();

  assert.match(app.element('update-error').textContent, /Download interrupted/);
  assert.equal(app.element('update-error').hidden, false);
  assert.equal(app.element('update-progress').hidden, true);
  assert.ok(app.controls.every((control) => !control.disabled));
  assert.equal(app.callsFor('status').length, statusCalls + 1);
  assert.match(app.element('local-status').textContent, /^Running\./);

  const retrying = app.fire('update-install');
  assert.equal(app.callsFor('install_update').length, 2);
  assert.equal(app.element('update-error').hidden, true);
  assert.equal(app.element('update-progress').textContent, 'Preparing…');
  assert.ok(app.controls.every((control) => control.disabled));
  app.callsFor('install_update')[1].resolve();
  await retrying;
});

test('a pending local action blocks duplicate actions and update installation', async () => {
  const app = launcher();
  await offerUpdate(app);
  const opening = app.fire('local-open');
  assert.equal(app.element('local-open').disabled, true);
  assert.equal(app.element('update-install').disabled, true);
  const blocked = [app.fire('local-open'), app.fire('update-install')];
  assert.equal(app.callsFor('open_local').length, 1);
  assert.equal(app.callsFor('install_update').length, 0);
  await Promise.all(blocked);

  app.callsFor('open_local')[0].resolve();
  await opening;
  await setImmediate();
  assert.equal(app.element('local-open').disabled, false);
  assert.equal(app.element('update-install').disabled, false);
  const installing = app.fire('update-install');
  assert.equal(app.callsFor('install_update').length, 1);
  app.callsFor('install_update')[0].resolve();
  await installing;
});
