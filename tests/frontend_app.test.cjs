const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/static/app.js'), 'utf8');

class Element {
  constructor(tag = 'div') {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this.listeners = new Map();
    this.attributes = new Map();
    this.dataset = {};
    this.className = '';
    this.value = '';
    this.hidden = false;
    this.disabled = false;
    this.open = false;
    this.style = {};
    this.scrollTop = 0;
    this.clientHeight = 290;
    this.scrollHeight = 290;
    this.classList = {
      add: (...names) => names.forEach(name => this.classList._set.add(name)),
      remove: (...names) => names.forEach(name => this.classList._set.delete(name)),
      toggle: (name, force) => {
        const enabled = force === undefined ? !this.classList._set.has(name) : force;
        if (enabled) this.classList._set.add(name);
        else this.classList._set.delete(name);
        return enabled;
      },
      contains: name => this.classList._set.has(name),
      _set: new Set(),
    };
    this.elements = { namedItem: name => this.namedItems?.get(name) || new Element('input') };
  }

  addEventListener(name, handler) {
    const handlers = this.listeners.get(name) || [];
    handlers.push(handler);
    this.listeners.set(name, handlers);
  }

  dispatch(name, event = {}) {
    if (this.disabled) return;
    for (const handler of this.listeners.get(name) || []) {
      handler({ preventDefault() {}, stopPropagation() {}, target: this, ...event });
    }
  }

  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = [...children]; }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.get(name) ?? null; }
  removeAttribute(name) { this.attributes.delete(name); }
  querySelector(selector) {
    if (selector === '[type="submit"]') return this.submitButton || null;
    if (selector === '.result-card') return this.children.find(child => child.className.includes('result-card')) || null;
    return null;
  }
  querySelectorAll(selector) {
    if (selector === '.result-card') return this.children.filter(child => child.className.includes('result-card'));
    return [];
  }
  focus() { this.focused = true; }
  select() { this.textSelected = true; }
  scrollIntoView() {}
  showModal() { this.open = true; }
  close() { this.open = false; }
}

class FormDataMock {
  constructor(form) { this.form = form; }
  get(name) {
    const control = this.form.elements.namedItem(name);
    if (control.type === 'checkbox' && !control.checked) return null;
    return control.value;
  }
}

function deferred() {
  let resolve;
  let reject;
  let settled = false;
  const promise = new Promise((res, rej) => {
    resolve = value => { settled = true; res(value); };
    reject = error => { settled = true; rej(error); };
  });
  return { promise, resolve, reject, get settled() { return settled; } };
}

async function flushAsyncWork() {
  await new Promise(resolve => setImmediate(resolve));
}

function createApp() {
  const ids = ['root-list','add-root','add-folder-welcome','root-add-dialog','root-add-form','root-add-path','root-add-feedback','root-add-cancel','root-add-submit','root-path-suggestions','rescan-top','index-progress','index-progress-track','progress-label','progress-detail','cancel-index','engine-card','engine-title','engine-message','open-settings','search-form','query','search-hint','results-view','welcome','library-idle','idle-title','idle-message','start-index-empty','results-title','results-list','preview','extension-filter','answer-request','answer-view','answer-text','answer-sources','answer-state','library-summary','settings-dialog','settings-form','settings-feedback','download-embedding','download-chat','download-ocr','ocr-status','download-progress','download-progress-fill','download-progress-text','folder-scope','folder-scope-path','folder-scope-note','folder-scope-index','folder-scope-clear','toast'];
  const elements = new Map(ids.map(id => [id, new Element(id === 'query' ? 'input' : 'div')]));
  const submitButton = new Element('button');
  elements.get('settings-form').submitButton = submitButton;
  elements.get('settings-form').namedItems = new Map([
    'embedding_model','chat_model','n_gpu_layers','context_size','scan_interval','max_file_mb','filename_only_dirs','gpu_mode','chat_enabled',
  ].map(key => [key, new Element('input')]));
  elements.get('settings-form').namedItems.get('chat_enabled').type = 'checkbox';
  elements.get('settings-form').namedItems.get('chat_enabled').value = 'true';
  const documentListeners = new Map();
  const windowListeners = new Map();
  const modeButtons = ['hybrid', 'keyword'].map(mode => {
    const button = new Element('button');
    button.dataset.mode = mode;
    return button;
  });
  const document = {
    activeElement: null,
    getElementById: id => elements.get(id) || null,
    createElement: tag => new Element(tag),
    querySelectorAll: selector => selector === '.mode-button' ? modeButtons : [],
    addEventListener(name, handler) { documentListeners.set(name, handler); },
  };
  const timers = new Map();
  let nextTimer = 0;
  const setTimer = (handler, delay) => {
    const id = ++nextTimer;
    timers.set(id, { handler, delay });
    return id;
  };
  const clearTimer = id => timers.delete(id);
  const calls = [];
  const window = {
    location: { hash: '', pathname: '/', search: '' },
    addEventListener(name, handler) { windowListeners.set(name, handler); },
    moa: {
      invoke(command, args) {
        const request = deferred();
        calls.push({ command, args, ...request, isSettled: () => request.settled });
        return request.promise;
      },
    },
  };
  vm.runInNewContext(source, {
    window,
    document,
    location: window.location,
    history: { replaceState(_state, _title, url) { window.location.hash = String(url).includes('#') ? `#${String(url).split('#')[1]}` : ''; } },
    URLSearchParams,
    FormData: FormDataMock,
    Intl,
    setTimeout: setTimer,
    clearTimeout: clearTimer,
    console: { warn() {} },
  });

  function call(command) { return calls.filter(entry => entry.command === command); }
  function runTimer(delay) {
    const entry = [...timers.entries()].find(([, timer]) => timer.delay === delay);
    assert.ok(entry, `expected a ${delay}ms timer`);
    timers.delete(entry[0]);
    entry[1].handler();
  }
  function resolveStatus(overrides = {}) {
    const request = call('status').find(entry => !entry.isSettled());
    assert.ok(request, 'expected a pending status request');
    request.resolve({
      roots: [], counts: { files: 0, chunks: 0, vectors: 0 },
      index: { running: false }, engine: { embedding_ready: false, chat_ready: true }, settings: { chat_enabled: true, gpu_mode: 'auto' }, ocr: {}, model_download: {},
      ...overrides,
    });
  }
  return { elements, calls, call, runTimer, resolveStatus, document, modeButtons, documentListeners, window, windowListeners };
}

function searchResult(id, name) {
  return { id, name, path: `C:\\docs\\${name}`, extension: 'txt', size: 12, modified: '', snippet: '내용' };
}

function listResult(id) {
  return { id, name: `file-${String(id).padStart(4, '0')}.txt`, path: `C:\\docs\\file-${String(id).padStart(4, '0')}.txt`, extension: 'txt', size: 12, modified: '' };
}

function textTree(node) {
  return [node.textContent || '', ...(node.children || []).map(textTree)].join(' ');
}

test('opens the current file with Enter and navigates instant results without waiting for AI', async () => {
  const app = createApp();
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }], engine: { embedding_ready: true } });
  await flushAsyncWork();
  const query = app.elements.get('query');
  query.value = '작업';
  app.elements.get('search-form').dispatch('submit');
  app.call('search')[0].resolve({ results: [searchResult(1, 'first.txt'), searchResult(2, 'second.txt')] });
  await flushAsyncWork();
  assert.equal(app.call('search').at(-1).args.mode, 'hybrid');
  assert.equal(app.call('search').at(-1).isSettled(), false);
  const rows = app.elements.get('results-list').querySelectorAll('.result-card');
  assert.equal(rows[0].attributes.get('role'), 'option');
  assert.equal(rows[0].attributes.get('aria-selected'), 'true');
  assert.equal(rows[0].children.length, 6, 'the list shows columns rather than body snippets');
  query.dispatch('keydown', { key: 'ArrowDown' });
  assert.equal(rows[1].attributes.get('aria-selected'), 'true');
  query.dispatch('keydown', { key: 'Enter' });
  assert.equal(app.call('open_file').at(-1).args.id, 2);
  query.dispatch('keydown', { key: 'Enter', ctrlKey: true });
  assert.equal(app.call('reveal_file').at(-1).args.id, 2);
  query.dispatch('keydown', { key: 'ArrowUp' });
  app.call('file').at(-1).resolve({ ...searchResult(1, 'first.txt'), passages: [{ text: '최신 본문' }] });
  await flushAsyncWork();
  app.call('file')[0].resolve({ ...searchResult(1, 'first.txt'), passages: [{ text: '이전 본문' }] });
  await flushAsyncWork();
  assert.match(textTree(app.elements.get('preview')), /최신 본문/);
  assert.doesNotMatch(textTree(app.elements.get('preview')), /이전 본문/, 'a late preview for the same file cannot replace a newer one');
  rows[0].dispatch('dblclick');
  assert.equal(app.call('open_file').at(-1).args.id, 1);
  query.value = '새 검색';
  query.dispatch('input');
  const openCount = app.call('open_file').length;
  query.dispatch('keydown', { key: 'Enter' });
  assert.equal(app.call('open_file').length, openCount, 'a changed input cannot open an old result');
});

test('uses desktop search shortcuts without taking focus from a settings dialog', async () => {
  const app = createApp();
  app.resolveStatus();
  await flushAsyncWork();
  const query = app.elements.get('query');
  for (const event of [{ key: 'f', ctrlKey: true }, { key: 'F3' }]) {
    query.focused = false;
    query.textSelected = false;
    app.documentListeners.get('keydown')({ ...event, preventDefault() {} });
    assert.equal(query.focused, true);
    assert.equal(query.textSelected, true);
  }
  query.focused = false;
  app.elements.get('settings-dialog').open = true;
  app.documentListeners.get('keydown')({ key: 'f', ctrlKey: true, preventDefault() {} });
  assert.equal(query.focused, false);
  const html = fs.readFileSync(path.join(__dirname, '../src/static/index.html'), 'utf8');
  assert.doesNotMatch(html, /id="(?:quit-dialog|confirm-quit|quit-app)"/, 'the native close button needs no web confirmation');
});

test('loads the blank-query file list in cursor pages while virtual rows preserve selection and DOM', async () => {
  const app = createApp();
  app.resolveStatus({
    roots: [{ id: 1, path: 'C:\\docs' }],
    list_revision: 7,
    settings: { chat_enabled: true, gpu_mode: 'auto' },
    engine: { embedding_ready: true, chat_ready: true },
  });
  await flushAsyncWork();
  const list = app.elements.get('results-list');
  const firstPage = app.call('list_files')[0];
  const cursor = { name: 'file-0200.txt', path: 'C:\\docs\\file-0200.txt', id: 200, revision: 7 };
  firstPage.resolve({ results: Array.from({ length: 200 }, (_, index) => listResult(index + 1)), total: 500, next_cursor: cursor, revision: 7 });
  await flushAsyncWork();

  assert.equal(firstPage.args.limit, 200);
  assert.equal(firstPage.args.query, undefined, 'browse uses the dedicated list command, not an empty search');
  assert.match(app.elements.get('results-title').textContent, /파일 500개 · 200개 불러옴/);
  app.runTimer(12000);
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }], list_revision: 7 });
  await flushAsyncWork();
  assert.match(app.elements.get('results-title').textContent, /파일 500개 · 200개 불러옴/, 'same-revision status polls preserve browse counts');
  assert.equal(app.call('list_files').length, 1, 'same-revision status polls do not reload browse pages');
  let rows = list.querySelectorAll('.result-card');
  assert.ok(rows.length <= 100, `virtual rendering caps rows, got ${rows.length}`);
  assert.match(textTree(rows[0]), /—/, 'browse does not claim a filename match');
  assert.equal(app.elements.get('answer-request').disabled, true, 'browse results never enable AI answers');
  app.elements.get('answer-request').dispatch('click');
  assert.equal(app.call('answer').length, 0);

  list.scrollHeight = 100000;
  list.scrollTop = 5500;
  list.dispatch('scroll');
  rows = list.querySelectorAll('.result-card');
  const selected = rows.find(row => row.getAttribute('data-result-id') === '191');
  assert.ok(selected);
  selected.dispatch('click');
  assert.equal(selected.getAttribute('aria-selected'), 'true');
  list.scrollHeight = 200 * 29;
  list.scrollTop = list.scrollHeight - list.clientHeight - 10;
  list.dispatch('scroll');
  const secondPage = app.call('list_files')[1];
  assert.deepEqual(secondPage.args.cursor, cursor);
  secondPage.resolve({ results: Array.from({ length: 200 }, (_, index) => listResult(index + 201)), total: 500, next_cursor: { name: 'file-0400.txt', path: 'C:\\docs\\file-0400.txt', id: 400, revision: 7 }, revision: 7 });
  await flushAsyncWork();
  rows = list.querySelectorAll('.result-card');
  assert.ok(rows.length <= 100);
  assert.ok(rows.includes(selected), 'page append keeps unchanged visible row nodes');
  assert.equal(selected.getAttribute('aria-selected'), 'true', 'page append keeps selected file');
  assert.match(app.elements.get('results-title').textContent, /400개 불러옴/);
  assert.equal(list.scrollTop, list.scrollHeight - list.clientHeight - 10, 'append preserves the user scroll position');
  const pagesBeforeResize = app.call('list_files').length;
  list.clientHeight = 580;
  app.windowListeners.get('resize')();
  assert.equal(app.call('list_files').length, pagesBeforeResize, 'resizing only rerenders the virtual window');
  assert.ok(list.querySelectorAll('.result-card').length <= 100);
  app.elements.get('query').dispatch('keydown', { key: 'Enter' });
  assert.equal(app.call('open_file').at(-1).args.id, 191, 'Enter opens the selected browse row');
  app.elements.get('query').dispatch('keydown', { key: 'Enter', ctrlKey: true });
  assert.equal(app.call('reveal_file').at(-1).args.id, 191);

  list.clientHeight = 290;
  list.scrollHeight = 400 * 29;
  list.scrollTop = list.scrollHeight - list.clientHeight - 10;
  list.dispatch('scroll');
  const changedRevisionPage = app.call('list_files')[2];
  changedRevisionPage.resolve({ results: [], total: 510, next_cursor: null, revision: 8, reset_required: true });
  await flushAsyncWork();
  const restarted = app.call('list_files')[3];
  assert.equal(restarted.args.cursor, null, 'a changed database revision restarts from page one');
  assert.doesNotMatch(textTree(list), /file-0191/, 'the old page set is cleared during revision restart');
  restarted.resolve({ results: [listResult(501)], total: 510, next_cursor: null, revision: 8 });
  await flushAsyncWork();
  assert.match(textTree(list), /file-0501/);
});

test('index completion refreshes folder scope without resetting a stable browse list', async () => {
  const app = createApp();
  app.resolveStatus({
    roots: [{ id: 1, path: 'C:\\docs' }], list_revision: 11,
    index: { running: true },
  });
  await flushAsyncWork();

  app.window.location.hash = '#folder=C%3A%5Cdocs%5Cchild';
  app.windowListeners.get('hashchange')();
  app.call('scope')[0].resolve({ path: 'C:\\docs\\child', indexed: true, covered: true });
  await flushAsyncWork();
  app.call('list_files')[1].resolve({ results: [listResult(5)], total: 1, next_cursor: null, revision: 11 });
  await flushAsyncWork();

  app.runTimer(2800);
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }], list_revision: 11, index: { running: false } });
  await flushAsyncWork();
  assert.equal(app.call('list_files').length, 2, 'index completion with unchanged revision does not restart the list');
  const scopeRefresh = app.call('scope').at(-1);
  assert.equal(scopeRefresh.args.path, 'C:\\docs\\child');
  scopeRefresh.resolve({ path: 'C:\\docs\\child', indexed: true, covered: true, root_path: 'C:\\docs' });
  await flushAsyncWork();
  assert.match(app.elements.get('results-title').textContent, /파일 1개 · 1개 불러옴/);
  assert.match(textTree(app.elements.get('results-list')), /file-0005/);
});

test('coalesces immediate lexical input, delays hybrid, and ignores stale responses', async () => {
  const app = createApp();
  app.resolveStatus({ engine: { embedding_ready: true, chat_ready: true }, settings: { chat_enabled: true, gpu_mode: 'auto' } });
  await flushAsyncWork();
  const query = app.elements.get('query');
  query.value = '계';
  query.dispatch('input');
  assert.equal(app.call('search').length, 1, 'the first character is searched immediately');
  const first = app.call('search')[0];
  assert.equal(first.args.mode, 'instant');
  query.value = '계약';
  query.dispatch('input');
  query.value = '계약서';
  query.dispatch('input');
  assert.equal(app.call('search').length, 1, 'rapid keystrokes are coalesced behind the active request');
  first.resolve({ results: [searchResult(99, 'stale.txt')] });
  await flushAsyncWork();
  const latest = app.call('search')[1];
  assert.equal(latest.args.query, '계약서');
  assert.doesNotMatch(textTree(app.elements.get('results-list')), /stale\.txt/);
  latest.resolve({ results: [searchResult(1, 'contract.txt')] });
  await flushAsyncWork();
  assert.equal(app.call('search').length, 2);
  app.runTimer(200);
  await flushAsyncWork();
  assert.equal(app.call('search').length, 3, 'the hybrid timer reuses a completed instant request instead of sending it again');
  app.elements.get('search-form').dispatch('submit');
  await flushAsyncWork();
  assert.equal(app.call('search').length, 3, 'repeated Enter while hybrid search is pending reuses its in-flight request');
  const hybrid = app.call('search')[2];
  assert.equal(hybrid.args.mode, 'hybrid');
  assert.equal(hybrid.args.query, '계약서');
  hybrid.resolve({ results: [searchResult(2, 'semantic-contract.txt')] });
  await flushAsyncWork();
  assert.match(textTree(app.elements.get('results-list')), /semantic-contract\.txt/);
  assert.doesNotMatch(textTree(app.elements.get('results-list')), /stale\.txt/);
});

test('an old browse ArrowDown continuation cannot change selection after a new search', async () => {
  const app = createApp();
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }] });
  await flushAsyncWork();
  app.call('list_files')[0].resolve({ results: [listResult(1)], total: 2, next_cursor: { name: 'file-0002.txt', path: 'C:\\docs\\file-0002.txt', id: 2 }, revision: 1 });
  await flushAsyncWork();

  app.elements.get('query').dispatch('keydown', { key: 'ArrowDown', preventDefault() {} });
  const olderPage = app.call('list_files')[1];
  app.elements.get('query').value = '새 검색';
  app.elements.get('query').dispatch('input');
  app.call('search')[0].resolve({ results: [searchResult(70, '새결과.txt'), searchResult(71, '다른결과.txt')] });
  await flushAsyncWork();
  assert.equal(app.call('file').at(-1).args.id, 70);

  olderPage.resolve({ results: [listResult(2)], total: 2, next_cursor: null, revision: 1 });
  await flushAsyncWork();
  const selected = app.elements.get('results-list').querySelectorAll('.result-card').filter(row => row.getAttribute('aria-selected') === 'true');
  assert.equal(selected.length, 1);
  assert.equal(selected[0].getAttribute('data-result-id'), '70', 'the current search selection stays active after the obsolete page completes');
  assert.equal(app.call('file').at(-1).args.id, 70, 'the stale key handler does not request a preview for another file');
});

test('retries a failed browse page with the same cursor and preserves already loaded selection', async () => {
  const app = createApp();
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }] });
  await flushAsyncWork();
  const list = app.elements.get('results-list');
  const cursor = { name: 'file-0001.txt', path: 'C:\\docs\\file-0001.txt', id: 1, revision: 3 };
  app.call('list_files')[0].resolve({ results: [listResult(1)], total: 2, next_cursor: cursor, revision: 3 });
  await flushAsyncWork();
  const previewCalls = app.call('file').length;

  list.scrollHeight = 350;
  list.scrollTop = 0;
  list.dispatch('scroll');
  const failedPage = app.call('list_files')[1];
  failedPage.reject(new Error('temporary list failure'));
  await flushAsyncWork();
  assert.match(textTree(list), /file-0001/);
  assert.match(textTree(list), /다시 불러오기/);

  function findRetry(node) {
    if (node.tagName === 'BUTTON' && node.textContent === '다시 불러오기') return node;
    for (const child of node.children || []) { const found = findRetry(child); if (found) return found; }
    return null;
  }
  findRetry(list).dispatch('click');
  const retry = app.call('list_files')[2];
  assert.deepEqual(retry.args.cursor, cursor, 'retry continues at the failed page cursor');
  retry.resolve({ results: [listResult(2)], total: 2, next_cursor: null, revision: 3 });
  await flushAsyncWork();
  assert.match(textTree(list), /file-0001/);
  assert.match(textTree(list), /file-0002/);
  const selected = list.querySelectorAll('.result-card').filter(row => row.getAttribute('aria-selected') === 'true');
  assert.equal(selected.length, 1);
  assert.equal(selected[0].getAttribute('data-result-id'), '1');
  assert.equal(app.call('file').length, previewCalls, 'retry keeps the selected preview instead of reloading it');
});

test('IME composition sends one completed query and composition Enter never opens a file', async () => {
  const app = createApp();
  app.resolveStatus({ engine: { embedding_ready: true } });
  await flushAsyncWork();
  const query = app.elements.get('query');
  query.dispatch('compositionstart');
  query.value = '회의';
  query.dispatch('input', { isComposing: true });
  query.dispatch('keydown', { key: 'Enter', isComposing: true, keyCode: 229 });
  assert.equal(app.call('search').length, 0, 'IME intermediate text is not sent');
  assert.equal(app.call('open_file').length, 0, 'IME confirm Enter does not open a file');
  query.dispatch('compositionend');
  query.dispatch('input', { isComposing: false });
  assert.equal(app.call('search').length, 1, 'compositionend dispatches one immediate lexical search');
  assert.equal(app.call('search')[0].args.query, '회의');
});

test('filter and scope changes invalidate prior browse pages and reload the right folder', async () => {
  const app = createApp();
  app.resolveStatus({ roots: [{ id: 1, path: 'C:\\docs' }] });
  await flushAsyncWork();
  const stale = app.call('list_files')[0];
  app.elements.get('extension-filter').value = 'pdf';
  app.elements.get('extension-filter').dispatch('change');
  const filtered = app.call('list_files')[1];
  assert.equal(filtered.args.extension, 'pdf');
  stale.resolve({ results: [listResult(1)], total: 1, next_cursor: null, revision: 1 });
  await flushAsyncWork();
  assert.doesNotMatch(textTree(app.elements.get('results-list')), /file-0001/);
  filtered.resolve({ results: [{ ...listResult(2), name: 'scan.pdf', extension: 'pdf' }], total: 1, next_cursor: null, revision: 2 });
  await flushAsyncWork();
  assert.match(textTree(app.elements.get('results-list')), /scan\.pdf/);

  app.window.location.hash = '#folder=C%3A%5Cdocs%5Cchild';
  app.windowListeners.get('hashchange')();
  const scopeRequest = app.call('scope')[0];
  scopeRequest.resolve({ path: 'C:\\docs\\child', indexed: true, covered: true });
  await flushAsyncWork();
  const scoped = app.call('list_files').at(-1);
  assert.equal(scoped.args.folder, 'C:\\docs\\child');
  scoped.resolve({ results: [{ ...listResult(3), name: 'scoped.pdf', extension: 'pdf' }], total: 1, next_cursor: null, revision: 3 });
  await flushAsyncWork();
  assert.match(textTree(app.elements.get('results-list')), /scoped\.pdf/);
  app.elements.get('folder-scope-clear').dispatch('click');
  const cleared = app.call('list_files').at(-1);
  assert.equal(cleared.args.folder, null, 'clearing scope restores the library listing');
});

test('follows the top semantic result until the user explicitly chooses a file', async () => {
  const app = createApp();
  app.resolveStatus({ engine: { embedding_ready: true } });
  await flushAsyncWork();
  app.elements.get('query').value = '영수증 합치기';
  app.elements.get('search-form').dispatch('submit');
  app.call('search')[0].resolve({ results: [searchResult(1, 'instant.txt')] });
  await flushAsyncWork();
  app.call('search')[1].resolve({ results: [searchResult(2, 'semantic.txt'), searchResult(1, 'instant.txt')] });
  await flushAsyncWork();
  assert.equal(app.elements.get('results-list').querySelectorAll('.result-card')[0].attributes.get('aria-selected'), 'true');
  app.elements.get('query').dispatch('keydown', { key: 'Enter' });
  assert.equal(app.call('open_file').at(-1).args.id, 2);

  app.elements.get('query').value = '영수증 합치기 추가';
  app.elements.get('query').dispatch('input');
  app.call('search')[2].resolve({ results: [searchResult(1, 'instant.txt'), searchResult(3, 'chosen.txt')] });
  await flushAsyncWork();
  app.elements.get('query').dispatch('keydown', { key: 'ArrowDown' });
  app.runTimer(200);
  await flushAsyncWork();
  app.call('search')[3].resolve({ results: [searchResult(2, 'semantic.txt'), searchResult(3, 'chosen.txt')] });
  await flushAsyncWork();
  assert.equal(app.elements.get('results-list').querySelectorAll('.result-card')[1].attributes.get('aria-selected'), 'true');
  app.elements.get('query').dispatch('keydown', { key: 'Enter' });
  assert.equal(app.call('open_file').at(-1).args.id, 3, 'semantic refresh preserves a user selection');
});

test('shows embedding chunk progress together with files scanned', async () => {
  const app = createApp();
  app.resolveStatus({ index: {
    running: true, phase: 'embedding', scanned: 17, indexed: 4, skipped: 2, errors: 1,
    current_chunk: 3, total_chunks: 8, current_path: 'D:\\docs\\긴 본문.txt',
  } });
  await flushAsyncWork();
  assert.equal(app.elements.get('index-progress').hidden, false);
  assert.equal(app.elements.get('index-progress').classList.contains('running'), true);
  assert.equal(app.elements.get('index-progress-track').hidden, false);
  assert.equal(app.elements.get('cancel-index').hidden, false);
  assert.equal(app.elements.get('cancel-index').disabled, false);
  assert.match(app.elements.get('progress-label').textContent, /3\/8개 본문 조각 분석 중/);
  assert.match(app.elements.get('progress-detail').textContent, /17개 파일 확인/);
  assert.match(app.elements.get('progress-detail').textContent, /긴 본문\.txt/);
});

test('reports background filename, body/OCR, and embedding preparation', async () => {
  const phases = [
    { phase: 'metadata', metadata_done: 5, metadata_total: 12, expected: /파일명·경로 색인 중 · 5\/12개 파일/ },
    { phase: 'content', content_done: 2, content_total: 7, expected: /본문·OCR 분석 중 · 2\/7개 파일/ },
    { phase: 'embedding', embedding_done_chunks: 4, embedding_total_chunks: 13, expected: /EmbeddingGemma 2 Q8_0 · 4\/13개 본문 조각 분석 중/ },
  ];
  for (const phase of phases) {
    const app = createApp();
    app.resolveStatus({
      index: { running: true, scanned: 5, indexed: 1, skipped: 0, errors: 0, ...phase },
      engine: { embedding_ready: true, chat_ready: false, gpu_backend: 'CUDA', gpu_active: false, gpu_layers_offloaded: 0 },
      ocr: { ready: true },
    });
    await flushAsyncWork();
    assert.match(app.elements.get('progress-label').textContent, phase.expected);
    assert.match(app.elements.get('engine-card').title, phase.phase === 'metadata' ? /파일명 색인 중/ : /파일명 검색 · 위치 등록 대기/);
    assert.match(app.elements.get('engine-card').title, /PP-OCRv5 준비됨/);
    assert.equal(app.elements.get('engine-title').textContent, 'EmbeddingGemma 2 Q8_0');
    assert.match(app.elements.get('engine-message').textContent, /CUDA 모델 로드됨 · GPU 층 0개/);
  }
});

test('shows PDF extraction stage and page progress inside the content phase', async () => {
  const app = createApp();
  app.resolveStatus({ index: {
    running: true, phase: 'content', content_phase: 'pdf_ocr', content_done: 3, content_total: 9,
    current_page: 7, total_pages: 18, current_path: 'D:\\docs\\scan.pdf', scanned: 11, errors: 0,
  } });
  await flushAsyncWork();
  assert.match(app.elements.get('progress-label').textContent, /본문·OCR 분석 중 · 3\/9개 파일 · PDF에서 글자를 찾는 중/);
  assert.match(app.elements.get('progress-detail').textContent, /7\/18페이지/);
  assert.match(app.elements.get('progress-detail').textContent, /11개 파일 확인/);
});

test('keeps terminal index summaries visible without implying completion on errors or cancellation', async () => {
  const idle = createApp();
  idle.resolveStatus();
  await flushAsyncWork();
  assert.equal(idle.elements.get('index-progress').hidden, true, 'idle status without a phase stays hidden');

  const cases = [
    { phase: 'complete', scanned: 24, indexed: 19, skipped: 5, errors: 0, reused: 3, reused_chunks: 22, label: /색인이 완료됐어요/, extra: null },
    { phase: 'partial', scanned: 24, indexed: 18, skipped: 4, errors: 2, last_error: '두 파일을 읽지 못했습니다.', label: /일부 파일을 읽지 못했어요/, extra: /두 파일을 읽지 못했습니다/ },
    { phase: 'error', scanned: 7, indexed: 0, skipped: 0, errors: 1, last_error: '디스크 오류', label: /실패했어요/, extra: /디스크 오류/ },
    { phase: 'cancelled', scanned: 11, indexed: 6, skipped: 1, errors: 0, label: /취소됐어요/, extra: null },
  ];
  for (const index of cases) {
    const app = createApp();
    const { label, extra, ...status } = index;
    app.resolveStatus({ index: { running: false, ...status } });
    await flushAsyncWork();
    assert.equal(app.elements.get('index-progress').hidden, false, `${index.phase} summary should stay visible`);
    assert.equal(app.elements.get('index-progress').classList.contains('running'), false);
    assert.equal(app.elements.get('index-progress').classList.contains('finished'), true);
    assert.equal(app.elements.get('index-progress-track').hidden, true);
    assert.equal(app.elements.get('cancel-index').hidden, true);
    assert.equal(app.elements.get('cancel-index').disabled, true);
    assert.match(app.elements.get('progress-label').textContent, label);
    assert.match(app.elements.get('progress-detail').textContent, /확인 \d+개 · 목록 갱신 \d+개 · 건너뜀 \d+개 · 오류 \d+개/);
    if (extra) assert.match(app.elements.get('progress-detail').textContent, extra);
    if (index.reused !== undefined) assert.match(app.elements.get('progress-detail').textContent, /재사용 3개 파일·22개 조각/);
    else assert.doesNotMatch(app.elements.get('progress-detail').textContent, /재사용 \d+개 파일/);
  }
});

test('distinguishes disabled or pending semantic indexing and unknown CUDA layer count', async () => {
  const disabled = createApp();
  disabled.resolveStatus({
    index: { running: false, phase: 'complete', scanned: 12, indexed: 8, skipped: 4, errors: 0, embedding_state: 'disabled', content_done: 5, content_total: 9, embedding_done_chunks: 0, embedding_total_chunks: 0 },
    engine: { embedding_ready: false, chat_ready: false, gpu_backend: 'CUDA', gpu_active: false, gpu_layers_offloaded: null },
  });
  await flushAsyncWork();
  assert.match(disabled.elements.get('progress-label').textContent, /파일명·본문 색인 완료 · 의미 색인 꺼짐/);
  assert.match(disabled.elements.get('progress-detail').textContent, /목록 갱신 8개 · 건너뜀 4개 · 오류 0개 · 본문 5\/9개 파일 · 의미 색인 꺼짐/);
  assert.match(disabled.elements.get('engine-message').textContent, /CUDA 모델 로드됨 · GPU 층 수 확인 불가/);
  assert.doesNotMatch(disabled.elements.get('engine-message').textContent, /모델 실행 전/);

  const pending = createApp();
  pending.resolveStatus({
    index: { running: false, phase: 'complete', scanned: 12, indexed: 8, skipped: 4, errors: 0, embedding_state: 'pending', content_done: 9, content_total: 9, embedding_done_chunks: 6, embedding_total_chunks: 10 },
  });
  await flushAsyncWork();
  assert.match(pending.elements.get('progress-label').textContent, /의미 색인이 남아 있어요/);
  assert.match(pending.elements.get('progress-detail').textContent, /본문 9\/9개 파일 · 의미 대기 6\/10개 조각/);
});

test('disables stale questions immediately and only enables them after the latest search', async () => {
  const app = createApp();
  const query = app.elements.get('query');
  const form = app.elements.get('search-form');
  const question = app.elements.get('answer-request');
  const answer = app.elements.get('answer-view');
  app.resolveStatus();
  await flushAsyncWork();

  query.value = '첫 검색';
  form.dispatch('submit');
  const firstSearch = app.call('search')[0];
  assert.equal(question.disabled, true, 'question stays disabled while a search is pending');
  firstSearch.resolve({ results: [searchResult(1, 'first.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, false, 'the completed current search enables questions');

  question.dispatch('click');
  assert.equal(app.call('answer').length, 1);
  assert.equal(answer.hidden, false);

  query.value = '두 번째 검색';
  query.dispatch('input');
  assert.equal(question.disabled, true, 'editing the query disables questions before debounce');
  assert.equal(answer.hidden, true, 'editing the query hides an answer for the previous query');
  assert.equal(app.elements.get('results-title').textContent, '파일 찾는 중…');
  assert.equal(app.elements.get('results-list').children.length, 1, 'the previous result cards are cleared immediately');
  assert.match(textTree(app.elements.get('results-list')), /파일명과 본문을 찾고 있어요/);
  assert.match(textTree(app.elements.get('preview')), /파일을 선택해 주세요/, 'the previous file preview is cleared immediately');
  question.dispatch('click');
  assert.equal(app.call('answer').length, 1, 'a stale question cannot be submitted during debounce');

  form.dispatch('submit');
  const secondSearch = app.call('search')[1];
  query.value = '세 번째 검색';
  query.dispatch('input');
  secondSearch.resolve({ results: [searchResult(2, 'stale.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, true, 'a stale response cannot re-enable the question button');

  const thirdSearch = app.call('search')[2];
  assert.equal(question.disabled, true, 'question remains disabled during the replacement search');
  thirdSearch.resolve({ results: [searchResult(3, 'third.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, false, 'only the completed replacement search enables questions');

  query.value = '네 번째 검색';
  query.dispatch('input');
  form.dispatch('submit');
  const fourthSearch = app.call('search')[3];
  query.value = '다섯 번째 검색';
  query.dispatch('input');
  form.dispatch('submit');
  assert.equal(app.call('search').length, 4, 'only one fast search is in flight while the latest input is coalesced');
  fourthSearch.resolve({ results: [] });
  await flushAsyncWork();
  const fifthSearch = app.call('search')[4];
  assert.equal(fifthSearch.args.query, '다섯 번째 검색');
  fifthSearch.resolve({ results: [searchResult(5, 'fifth.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, false);
  assert.equal(question.disabled, false, 'a late stale response cannot disable the latest completed search');
  question.dispatch('click');
  assert.equal(app.call('answer').at(-1).args.query, '다섯 번째 검색');
});

test('shows instant lexical matches before hybrid results and ignores a stale semantic response', async () => {
  const app = createApp();
  app.resolveStatus({
    engine: { embedding_ready: true, chat_ready: true },
    settings: { chat_enabled: true, gpu_mode: 'auto' },
  });
  await flushAsyncWork();
  const query = app.elements.get('query');
  const form = app.elements.get('search-form');
  query.value = '계약 갱신 기간';
  form.dispatch('submit');
  const instant = app.call('search')[0];
  assert.equal(instant.args.mode, 'instant');
  instant.resolve({ results: [searchResult(1, 'contract.txt')] });
  await flushAsyncWork();
  assert.match(app.elements.get('results-title').textContent, /파일명·키워드 결과 1개 · 의미 검색 중/);
  assert.match(textTree(app.elements.get('results-list')), /결과를 먼저 보여드리고/);
  assert.equal(app.elements.get('answer-request').disabled, true);
  const semantic = app.call('search')[1];
  assert.equal(semantic.args.mode, 'hybrid');

  query.value = '지난 회의 예산';
  form.dispatch('submit');
  const nextInstant = app.call('search')[2];
  semantic.resolve({ results: [searchResult(99, 'stale.txt')] });
  await flushAsyncWork();
  assert.match(app.elements.get('results-title').textContent, /찾는 중/);
  nextInstant.resolve({ results: [searchResult(2, 'budget.txt')] });
  await flushAsyncWork();
  const nextSemantic = app.call('search')[3];
  nextSemantic.resolve({ results: [searchResult(3, 'minutes.txt')] });
  await flushAsyncWork();
  assert.match(app.elements.get('results-title').textContent, /“지난 회의 예산”의 결과 1개/);
  assert.match(textTree(app.elements.get('results-list')), /minutes\.txt/);
  assert.doesNotMatch(textTree(app.elements.get('results-list')), /stale\.txt/);
});

test('explains hybrid versus keyword search and preserves each match signal in result labels', async () => {
  const app = createApp();
  const hint = app.elements.get('search-hint');
  assert.match(hint.textContent, /의미 검색은 검색어가 없는 문서도 포함/);
  app.modeButtons[1].dispatch('click');
  assert.match(hint.textContent, /파일명·경로와 본문에서 찾습니다/);

  app.resolveStatus();
  await flushAsyncWork();
  const query = app.elements.get('query');
  query.value = '표시 신호';
  app.elements.get('search-form').dispatch('submit');
  const request = app.call('search')[0];
  assert.equal(request.args.mode, 'keyword');
  request.resolve({ intent: 'source_code', results: [
    { ...searchResult(1, 'filename.txt'), match_type: 'filename' },
    { ...searchResult(2, 'body.txt'), match_type: 'keyword' },
    { ...searchResult(3, 'semantic.txt'), match_type: 'semantic' },
    { ...searchResult(4, 'combined.txt'), match_type: 'filename+keyword+semantic' },
    { ...searchResult(5, 'source.rs'), match_type: 'filetype+filename' },
  ] });
  await flushAsyncWork();
  const cards = app.elements.get('results-list').children.filter(child => child.className.includes('result-card'));
  assert.equal(cards.length, 5);
  assert.match(textTree(cards[0]), /파일명·경로/);
  assert.match(textTree(cards[1]), /본문 키워드/);
  assert.match(textTree(cards[2]), /의미 유사/);
  assert.match(textTree(cards[3]), /파일명·경로 \+ 본문 키워드 \+ 의미 유사/);
  assert.match(textTree(cards[4]), /소스코드 파일 \+ 파일명·경로/);
  assert.equal(hint.textContent, '소스코드 파일과 검색어가 포함된 문서를 찾습니다.');

  query.value = '일반 검색';
  query.dispatch('input');
  assert.match(hint.textContent, /파일명·경로와 본문에서 찾습니다/);
});

test('restores and saves filename-only folder settings including an empty value', async () => {
  const app = createApp();
  app.resolveStatus({ settings: { filename_only_dirs: '_site,temp,vendor' } });
  await flushAsyncWork();
  app.elements.get('open-settings').dispatch('click');
  const input = app.elements.get('settings-form').elements.namedItem('filename_only_dirs');
  assert.equal(input.value, '_site,temp,vendor');
  input.value = '';
  app.elements.get('settings-form').dispatch('submit');
  const update = app.call('update_settings')[0];
  assert.equal(update.args.settings.filename_only_dirs, '');
});

test('keeps Gemma answers and their download option off until enabled', async () => {
  const app = createApp();
  app.resolveStatus({
    settings: { chat_enabled: false, gpu_mode: 'auto' },
    engine: { embedding_ready: false, chat_ready: false, gpu_backend: 'unavailable' },
  });
  await flushAsyncWork();
  assert.equal(app.elements.get('answer-request').hidden, true);
  assert.equal(app.elements.get('download-chat').hidden, true);
  app.elements.get('open-settings').dispatch('click');
  const settings = app.elements.get('settings-form');
  const enabled = settings.elements.namedItem('chat_enabled');
  assert.equal(enabled.checked, false);
  enabled.checked = true;
  settings.dispatch('submit');
  const update = app.call('update_settings')[0];
  assert.equal(update.args.settings.chat_enabled, true);
  assert.equal(update.args.settings.gpu_mode, 'auto');
  update.resolve({});
  await flushAsyncWork();

  app.resolveStatus({
    settings: { chat_enabled: true, gpu_mode: 'cpu' },
    engine: { embedding_ready: false, chat_ready: false, gpu_backend: 'CPU', compute_device: 'CPU', gpu_active: false },
  });
  await flushAsyncWork();
  assert.equal(app.elements.get('answer-request').hidden, false);
  assert.equal(app.elements.get('answer-request').disabled, true);
  assert.equal(app.elements.get('download-chat').hidden, false);
  assert.match(app.elements.get('answer-request').title, /Gemma 4/);
  assert.match(app.elements.get('engine-message').textContent, /CPU 실행 중/);
});

test('explains when a selected folder has filename-only indexing', async () => {
  const app = createApp();
  app.resolveStatus();
  await flushAsyncWork();
  app.elements.get('query').value = '배포 파일';
  app.elements.get('search-form').dispatch('submit');
  app.call('search')[0].resolve({ results: [searchResult(14, 'index.html')] });
  await flushAsyncWork();
  app.call('file')[0].resolve({
    ...searchResult(14, 'index.html'),
    reader_id: 'filename-only-v1:text-v1',
    passages: [],
  });
  await flushAsyncWork();
  assert.match(textTree(app.elements.get('preview')), /설정에 따라 파일명·경로만 색인했어요/);
  assert.doesNotMatch(textTree(app.elements.get('preview')), /색인이 아직 진행 중/);
});

test('answer request rechecks the live input against the completed search query', async () => {
  const app = createApp();
  const query = app.elements.get('query');
  const form = app.elements.get('search-form');
  const question = app.elements.get('answer-request');
  app.resolveStatus();
  await flushAsyncWork();

  query.value = '검색어';
  form.dispatch('submit');
  app.call('search')[0].resolve({ results: [searchResult(1, 'result.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, false);

  // Simulate a programmatic/input race that changes the live value before the handler.
  query.value = '다른 검색어';
  question.listeners.get('click')[0]();
  assert.equal(app.call('answer').length, 0);
});
