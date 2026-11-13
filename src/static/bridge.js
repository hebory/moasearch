(() => {
  'use strict';

  const commands = Object.freeze({
    status: new Set(),
    scope: new Set(['path']),
    add_root: new Set(['path']),
    remove_root: new Set(['id']),
    start_index: new Set(),
    cancel_index: new Set(),
    update_settings: new Set(['settings']),
    search: new Set(['query', 'limit', 'mode', 'extension', 'folder']),
    answer: new Set(['query', 'limit', 'folder']),
    file: new Set(['id']),
    open_file: new Set(['id']),
    reveal_file: new Set(['id']),
    download_model: new Set(['kind']),
    quit: new Set(),
  });
  const maxMessageBytes = 32 * 1024;
  const maxPending = 64;
  const timeouts = Object.freeze({ search: 10 * 60 * 1000, answer: 10 * 60 * 1000 });
  const pending = new Map();
  const session = createSession();
  let nextId = 0;
  let closed = false;

  function createSession() {
    const bytes = new Uint8Array(16);
    if (!window.crypto?.getRandomValues) {
      throw new Error('앱 통신을 안전하게 시작할 수 없어요. 모아검색을 다시 실행해 주세요.');
    }
    window.crypto.getRandomValues(bytes);
    return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
  }

  function validate(command, args) {
    const fields = typeof command === 'string' && Object.hasOwn(commands, command)
      ? commands[command]
      : null;
    if (!fields || !args || typeof args !== 'object' || Array.isArray(args)) {
      throw new Error('지원하지 않는 앱 요청입니다.');
    }
    if (Object.keys(args).some(key => !fields.has(key))) {
      throw new Error('앱 요청 형식이 올바르지 않습니다.');
    }
  }

  function settle(id, entry, error, value) {
    if (pending.get(id) !== entry) return;
    pending.delete(id);
    clearTimeout(entry.timer);
    if (error) entry.reject(error instanceof Error ? error : new Error(String(error)));
    else entry.resolve(value);
  }

  function resolve(response) {
    if (!response || typeof response !== 'object' || response.session !== session) return;
    const entry = pending.get(response.id);
    if (!entry || typeof response.ok !== 'boolean') return;
    if (!response.ok) {
      settle(response.id, entry, typeof response.error === 'string' ? response.error : '요청을 완료하지 못했어요.');
      return;
    }
    settle(response.id, entry, null, response.value);
  }

  function invoke(command, args = {}) {
    if (closed) return Promise.reject(new Error('모아검색 창이 닫혔어요.'));
    if (!window.ipc || typeof window.ipc.postMessage !== 'function') {
      return Promise.reject(new Error('모아검색 앱과 연결되지 않았어요. 앱을 다시 실행해 주세요.'));
    }
    try {
      validate(command, args);
    } catch (error) {
      return Promise.reject(error);
    }
    if (pending.size >= maxPending) return Promise.reject(new Error('앱 요청이 너무 많아요. 잠시 후 다시 시도해 주세요.'));

    const id = ++nextId;
    let message;
    try {
      message = JSON.stringify({ id, session, command, args });
    } catch {
      return Promise.reject(new Error('앱 요청 형식이 올바르지 않습니다.'));
    }
    if (new TextEncoder().encode(message).byteLength > maxMessageBytes) {
      return Promise.reject(new Error('요청이 너무 커요. 내용을 줄여 주세요.'));
    }

    return new Promise((resolvePromise, rejectPromise) => {
      const entry = {
        resolve: resolvePromise,
        reject: rejectPromise,
        timer: setTimeout(() => settle(id, entry, '앱 응답을 기다리는 시간이 초과됐어요. 다시 시도해 주세요.'), timeouts[command] || 120_000),
      };
      pending.set(id, entry);
      try {
        window.ipc.postMessage(message);
      } catch {
        settle(id, entry, '앱에 요청을 전달하지 못했어요. 모아검색을 다시 실행해 주세요.');
      }
    });
  }

  function close() {
    if (closed) return;
    closed = true;
    for (const [id, entry] of pending) settle(id, entry, '모아검색 창이 닫혔어요.');
  }

  window.addEventListener('unload', close, { once: true });
  Object.defineProperty(window, '__moaResolve', {
    value: resolve,
    configurable: false,
    enumerable: false,
    writable: false,
  });
  Object.defineProperty(window, 'moa', {
    value: Object.freeze({ invoke }),
    configurable: false,
    enumerable: false,
    writable: false,
  });
})();
