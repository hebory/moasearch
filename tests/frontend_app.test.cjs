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
  focus() {}
  scrollIntoView() {}
  showModal() { this.open = true; }
  close() { this.open = false; }
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

async function flushAsyncWork() {
  await new Promise(resolve => setImmediate(resolve));
}

function createApp() {
  const ids = ['root-list','add-root','add-folder-welcome','root-add-dialog','root-add-form','root-add-path','root-add-feedback','root-add-cancel','root-add-submit','root-path-suggestions','rescan-top','quit-app','quit-dialog','cancel-quit','confirm-quit','index-progress','progress-label','progress-detail','cancel-index','engine-card','engine-title','engine-message','open-settings','search-form','query','search-hint','results-view','welcome','library-idle','idle-title','idle-message','start-index-empty','results-title','results-list','preview','extension-filter','answer-request','answer-view','answer-text','answer-sources','answer-state','library-summary','settings-dialog','settings-form','settings-feedback','download-embedding','download-chat','download-ocr','ocr-status','download-progress','download-progress-fill','download-progress-text','folder-scope','folder-scope-path','folder-scope-note','folder-scope-index','folder-scope-clear','toast'];
  const elements = new Map(ids.map(id => [id, new Element(id === 'query' ? 'input' : 'div')]));
  const submitButton = new Element('button');
  elements.get('settings-form').submitButton = submitButton;
  elements.get('settings-form').namedItems = new Map([
    'embedding_model','chat_model','n_gpu_layers','context_size','scan_interval','max_file_mb',
  ].map(key => [key, new Element('input')]));
  const documentListeners = new Map();
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
    addEventListener() {},
    moa: {
      invoke(command, args) {
        const request = deferred();
        calls.push({ command, args, ...request });
        return request.promise;
      },
    },
  };
  vm.runInNewContext(source, {
    window,
    document,
    location: window.location,
    history: { replaceState() {} },
    URLSearchParams,
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
  function resolveStatus() {
    call('status')[0].resolve({
      roots: [], counts: { files: 0, chunks: 0, vectors: 0 },
      index: { running: false }, engine: {}, settings: {}, ocr: {}, model_download: {},
    });
  }
  return { elements, calls, call, runTimer, resolveStatus, document, modeButtons };
}

function searchResult(id, name) {
  return { id, name, path: `C:\\docs\\${name}`, extension: 'txt', size: 12, modified: '', snippet: '내용' };
}

function textTree(node) {
  return [node.textContent || '', ...(node.children || []).map(textTree)].join(' ');
}

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
  assert.equal(app.elements.get('results-title').textContent, '검색 대기 중…');
  assert.equal(app.elements.get('results-list').children.length, 1, 'the previous result cards are cleared immediately');
  assert.match(textTree(app.elements.get('results-list')), /검색어 입력이 끝나면 검색합니다/);
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

  app.runTimer(500);
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
  const fifthSearch = app.call('search')[4];
  fifthSearch.resolve({ results: [searchResult(5, 'fifth.txt')] });
  await flushAsyncWork();
  assert.equal(question.disabled, false);
  fourthSearch.resolve({ results: [] });
  await flushAsyncWork();
  assert.equal(question.disabled, false, 'a late stale response cannot disable the latest completed search');
  question.dispatch('click');
  assert.equal(app.call('answer').at(-1).args.query, '다섯 번째 검색');
});

test('explains hybrid versus keyword search and preserves each match signal in result labels', async () => {
  const app = createApp();
  const hint = app.elements.get('search-hint');
  assert.match(hint.textContent, /의미 유사 검색은 입력한 단어가 없는 문서도 찾을 수/);
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
