const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/static/bridge.js'), 'utf8');

function createBridge({ available = true, postMessage } = {}) {
  const messages = [];
  const listeners = new Map();
  let randomByte = 0;
  const window = {
    crypto: {
      getRandomValues(bytes) {
        for (let index = 0; index < bytes.length; index += 1) bytes[index] = ++randomByte;
        return bytes;
      },
    },
    addEventListener(name, handler) {
      listeners.set(name, handler);
    },
  };
  if (available) {
    window.ipc = {
      postMessage(message) {
        messages.push(JSON.parse(message));
        postMessage?.(message);
      },
    };
  }
  vm.runInNewContext(source, { window, TextEncoder, setTimeout, clearTimeout, Uint8Array });
  return {
    window,
    messages,
    unload() { listeners.get('unload')?.(); },
  };
}

function respond(bridge, request, values) {
  bridge.window.__moaResolve({ id: request.id, session: request.session, ...values });
}

test('sends allowlisted command payloads and resolves responses out of order', async () => {
  const bridge = createBridge();
  assert.ok(Object.isFrozen(bridge.window.moa));

  const first = bridge.window.moa.invoke('search', {
    query: '계약', limit: 30, mode: 'hybrid', extension: null, folder: 'D:\\자료',
  });
  const second = bridge.window.moa.invoke('status');
  assert.equal(bridge.messages.length, 2);
  assert.equal(bridge.messages[0].command, 'search');
  assert.equal(bridge.messages[0].session, bridge.messages[1].session);
  assert.deepEqual(bridge.messages[0].args, {
    query: '계약', limit: 30, mode: 'hybrid', extension: null, folder: 'D:\\자료',
  });

  respond(bridge, bridge.messages[1], { ok: true, value: { counts: { files: 2 } } });
  respond(bridge, bridge.messages[0], { ok: true, value: { results: [] } });
  assert.deepEqual(await second, { counts: { files: 2 } });
  assert.deepEqual(await first, { results: [] });
});

test('rejects native errors as readable Error objects', async () => {
  const bridge = createBridge();
  const pending = bridge.window.moa.invoke('scope', { path: 'D:\\missing' });
  respond(bridge, bridge.messages[0], { ok: false, error: '폴더를 찾을 수 없습니다.' });
  await assert.rejects(pending, { message: '폴더를 찾을 수 없습니다.' });
});

test('ignores responses from another window session', async () => {
  const bridge = createBridge();
  const pending = bridge.window.moa.invoke('status');
  const request = bridge.messages[0];
  bridge.window.__moaResolve({ id: request.id, session: 'another-window', ok: true, value: 'wrong' });
  respond(bridge, request, { ok: true, value: 'right' });
  assert.equal(await pending, 'right');
});

test('rejects commands and fields outside the allowlist without sending IPC', async () => {
  const bridge = createBridge();
  await assert.rejects(bridge.window.moa.invoke('execute'), /지원하지 않는 앱 요청/);
  for (const command of ['constructor', 'toString', '__proto__']) {
    await assert.rejects(bridge.window.moa.invoke(command), /지원하지 않는 앱 요청/);
  }
  await assert.rejects(bridge.window.moa.invoke('search', { query: 'x', path: 'C:\\' }), /형식이 올바르지/);
  assert.equal(bridge.messages.length, 0);
});

test('rejects unserializable arguments without throwing outside the Promise', async () => {
  const bridge = createBridge();
  const circular = {};
  circular.self = circular;
  const circularRequest = bridge.window.moa.invoke('update_settings', { settings: circular });
  await assert.rejects(circularRequest, /형식이 올바르지/);
  const bigintRequest = bridge.window.moa.invoke('update_settings', { settings: { value: 1n } });
  await assert.rejects(bigintRequest, /형식이 올바르지/);
  assert.equal(bridge.messages.length, 0);
});

test('caps UTF-8 message size at 32 KiB', async () => {
  const bridge = createBridge();
  await assert.rejects(
    bridge.window.moa.invoke('search', { query: '가'.repeat(11_000) }),
    /요청이 너무 커요/,
  );
  assert.equal(bridge.messages.length, 0);
});

test('caps outstanding requests at 64', async () => {
  const bridge = createBridge();
  const pending = Array.from({ length: 64 }, () => bridge.window.moa.invoke('status'));
  await assert.rejects(bridge.window.moa.invoke('status'), /요청이 너무 많아요/);
  bridge.unload();
  await Promise.all(pending.map(request => assert.rejects(request, /창이 닫혔어요/)));
});

test('rejects requests when native IPC is unavailable or throws', async () => {
  const unavailable = createBridge({ available: false });
  await assert.rejects(unavailable.window.moa.invoke('status'), /앱과 연결되지 않았어요/);

  const throwing = createBridge({ postMessage() { throw new Error('internal'); } });
  await assert.rejects(throwing.window.moa.invoke('status'), /전달하지 못했어요/);
});

test('rejects outstanding requests on unload and prevents new requests', async () => {
  const bridge = createBridge();
  const pending = bridge.window.moa.invoke('answer', { query: '질문', limit: 5, folder: null });
  bridge.unload();
  await assert.rejects(pending, /창이 닫혔어요/);
  await assert.rejects(bridge.window.moa.invoke('status'), /창이 닫혔어요/);
});
