(() => {
  'use strict';
  const $ = (selector, scope = document) => scope.querySelector(selector);
  const dom = Object.fromEntries(['root-list','add-root','add-folder-welcome','root-add-dialog','root-add-form','root-add-path','root-add-feedback','root-add-cancel','root-add-submit','root-path-suggestions','rescan-top','index-progress','index-progress-track','progress-label','progress-detail','cancel-index','engine-card','engine-title','engine-message','open-settings','search-form','query','search-hint','results-view','welcome','library-idle','idle-title','idle-message','start-index-empty','results-title','results-list','preview','extension-filter','answer-request','answer-view','answer-text','answer-sources','answer-state','library-summary','settings-dialog','settings-form','settings-feedback','download-embedding','download-chat','download-ocr','ocr-status','download-progress','download-progress-fill','download-progress-text','folder-scope','folder-scope-path','folder-scope-note','folder-scope-index','folder-scope-clear','toast'].map(id => [id.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase()), document.getElementById(id)]));
  dom.answerRequest.disabled = true;
  const state = {roots:[], status:null, results:[], selected:null, selectionExplicit:false, mode:'hybrid', view:'browse', query:'', extension:'', searchTimer:0, hybridTimer:0, pollTimer:0, toastTimer:0, searchSequence:0, viewGeneration:0, answerSequence:0, scopeSequence:0, previewSequence:0, searchPending:false, folder:null, browse:{items:[],total:0,nextCursor:null,revision:null,loading:false,failed:false}, browseGeneration:0, virtualStart:0, virtualRowHeight:29, composing:false, instantInFlight:null, instantQueued:null, instantCompleted:null, hybridInFlight:null, addingRoot:false, indexingScope:false, closed:false, reportedIndexError:'', modelDownload:null, modelDownloadKind:'', reportedDownloadError:''};
  const esc = value => String(value ?? '');
  function element(tag, className, text) { const node = document.createElement(tag); if (className) node.className = className; if (text !== undefined) node.textContent = esc(text); return node; }
  function toast(message,error=false) { clearTimeout(state.toastTimer); dom.toast.textContent=message;dom.toast.classList.toggle('error',error);dom.toast.classList.add('show');state.toastTimer=setTimeout(()=>dom.toast.classList.remove('show'),3600); }
  function fmtBytes(size) { const n=Number(size)||0;if(n<1024)return `${n} B`;if(n<1024**2)return `${(n/1024).toFixed(0)} KB`;if(n<1024**3)return `${(n/1024**2).toFixed(1)} MB`;return `${(n/1024**3).toFixed(1)} GB`; }
  function fmtDate(value) { if(value===null||value===undefined||value==='')return '';let date;if(typeof value==='number'||/^\d{15,}$/.test(String(value))){const numeric=Number(value);date=new Date(Math.abs(numeric)>=1e14?numeric/1e6:numeric);}else date=new Date(value);return Number.isNaN(date.getTime())?String(value):new Intl.DateTimeFormat('ko-KR',{year:'numeric',month:'short',day:'numeric'}).format(date); }
  function fileIcon(ext) { const type=(ext||'file').replace(/^\./,'').toLowerCase();const box=element('span','file-icon',type.slice(0,5).toUpperCase());box.dataset.type=type;box.setAttribute('aria-hidden','true');return box; }
  function updateSearchHint() { dom.searchHint.textContent=state.mode==='keyword'?'입력한 단어를 파일명·경로와 본문에서 찾습니다.':'의미 검색은 검색어가 없는 문서도 포함합니다.'; }
  function answerAvailable() { return Boolean(state.view==='search'&&state.query&&state.status?.settings?.chat_enabled&&state.status?.engine?.chat_ready&&!state.searchPending&&state.results.length); }
  function humanPath(path) { return esc(path).replaceAll('\\','\\'); }
  function renderRoots(roots) {
    state.roots=roots||[];dom.rootList.replaceChildren();
    if(!state.roots.length){dom.rootList.append(element('div','root-empty','아직 추가한 위치가 없어요'));return;}
    for(const root of state.roots){const row=element('div','root-item');row.title=esc(root.path);const icon=element('span','root-icon','▱');icon.setAttribute('aria-hidden','true');const path=element('span','root-path',root.path);const remove=element('button','icon-button root-remove','×');remove.type='button';remove.setAttribute('aria-label',`${root.path} 검색 위치 제거`);remove.title='검색 위치 제거';remove.addEventListener('click',async event=>{event.stopPropagation();try{await window.moa.invoke('remove_root',{id:root.id});await refreshStatus();toast('검색 위치를 제거했어요.');}catch(error){toast(error.message,true);}});row.append(icon,path,remove);dom.rootList.append(row);}
  }
  function renderStatus(data) {
    if(state.closed)return;
    const wasIndexRunning=Boolean(state.status?.index?.running),wasChatEnabled=Boolean(state.status?.settings?.chat_enabled),previousListRevision=state.status?.list_revision;
    state.status=data;const counts=data.counts||{},index=data.index||{},engine=data.engine||{},settings=data.settings||{};
    renderRoots(data.roots||[]);
    const ready=Boolean(engine.embedding_ready);dom.engineCard.classList.toggle('ready',ready);dom.engineCard.classList.toggle('waiting',!ready);
    dom.engineTitle.textContent='EmbeddingGemma 2 Q8_0';
    const chatEnabled=Boolean(settings.chat_enabled);dom.answerRequest.hidden=!chatEnabled;dom.downloadChat.hidden=!chatEnabled;
    if(!chatEnabled){if(wasChatEnabled)state.answerSequence++;dom.answerView.hidden=true;}
    const gpuBackend=String(engine.gpu_backend||'unavailable'),rawGpuLayers=engine.gpu_layers_offloaded,gpuLayers=Number(rawGpuLayers)||0,gpuLayersKnown=rawGpuLayers!==null&&rawGpuLayers!==undefined&&Number.isFinite(Number(rawGpuLayers));
    const computeStatus=engine.gpu_active?`GPU 모델 로드됨 · ${engine.compute_device||gpuBackend} · ${gpuLayers}개 층`:gpuBackend==='CPU'?'CPU 실행 중':gpuBackend==='unavailable'?`연산 장치 확인 대기 · ${settings.gpu_mode==='cpu'?'CPU 지정':'자동 선택'}`:gpuBackend==='CUDA'?gpuLayersKnown?`CUDA 모델 로드됨 · GPU 층 ${gpuLayers}개`:'CUDA 모델 로드됨 · GPU 층 수 확인 불가':'연산 장치 확인 대기';
    const contentProgress=index.phase==='content'?` · 본문 ${Number(index.content_done)||0}/${Number(index.content_total)||0}`:'';
    const embeddingProgress=index.phase==='embedding'?` · 조각 ${Number(index.embedding_done_chunks)||Number(index.current_chunk)||0}/${Number(index.embedding_total_chunks)||Number(index.total_chunks)||0}`:'';
    const ocrStatus=data.ocr?.ready?'PP-OCRv5 준비됨':'OCR 모델 준비 필요';
    const fileNameStatus=index.phase==='metadata'?`파일명 색인 중 · ${Number(index.metadata_done)||0}/${Number(index.metadata_total)||0}개 파일`:Number(counts.files)>0||state.folder?.indexed?'파일명 검색 · 파일 준비됨':state.roots.length?'파일명 검색 · 색인 대기':'파일명 검색 · 위치 등록 대기';
    const chatStatus=chatEnabled?`Gemma 4 E2B IT QAT Q4_0 · ${engine.chat_ready?'준비됨':'준비 필요'}`:'Gemma 4 답변 · 꺼짐';
    dom.engineMessage.textContent=`${ready?'의미 검색 준비':'모델 준비 필요'} · ${computeStatus}`;
    dom.engineCard.title=`${fileNameStatus}\n본문 색인${contentProgress} · ${ocrStatus}\nEmbeddingGemma 2 Q8_0 · ${ready?'준비됨':'준비 필요'}${embeddingProgress}\n${chatStatus} · ${computeStatus}${engine.message?`\n${engine.message}`:''}`;
    dom.answerRequest.disabled=!answerAvailable();
    dom.answerRequest.title=!chatEnabled?'설정에서 답변 모델을 켜면 사용할 수 있어요.':!engine.chat_ready?'Gemma 4 E2B IT QAT Q4_0를 설정하고 설치해 주세요.':'';
    const running=Boolean(index.running),phase=String(index.phase||'');const terminalLabels={complete:'색인이 완료됐어요',partial:'색인이 끝났지만 일부 파일을 읽지 못했어요',error:'색인 작업에 실패했어요',cancelled:'색인이 취소됐어요'};const terminal=Object.hasOwn(terminalLabels,phase);const visible=running||terminal;const scanned=Number(index.scanned)||0,indexed=Number(index.indexed)||0,skipped=Number(index.skipped)||0,errors=Number(index.errors)||0;dom.indexProgress.hidden=!visible;dom.indexProgress.classList.toggle('running',running);dom.indexProgress.classList.toggle('finished',!running&&terminal);dom.cancelIndex.hidden=!running;dom.cancelIndex.disabled=!running;if(dom.indexProgressTrack)dom.indexProgressTrack.hidden=!running;
    if(running){const contentPhase=String(index.content_phase||'');const hasPageProgress=Number(index.current_page)>0&&Number(index.total_pages)>0;const pdf=hasPageProgress&&(contentPhase==='pdf_text'||contentPhase==='pdf_ocr');const embedding=phase==='embedding';const chunk=Number(index.embedding_done_chunks??index.current_chunk),totalChunks=Number(index.embedding_total_chunks??index.total_chunks);const chunkStatus=Number.isFinite(chunk)&&Number.isFinite(totalChunks)?`${Math.max(0,chunk)}/${Math.max(0,totalChunks)}개 본문 조각`:'본문 조각';const metadata=phase==='metadata';const content=phase==='content';const metadataProgress=`${Number(index.metadata_done)||0}/${Number(index.metadata_total)||0}개 파일`;const contentProgress=`${Number(index.content_done)||0}/${Number(index.content_total)||0}개 파일`;const contentStage=hasPageProgress?(contentPhase==='ocr'?'이미지 글자를 읽는 중':contentPhase==='pdf_text'?'PDF 텍스트를 읽는 중':contentPhase==='pdf_ocr'?'PDF에서 글자를 찾는 중':''):'';dom.progressLabel.textContent=metadata?`파일명·경로 색인 중 · ${metadataProgress}`:content?`본문·OCR 분석 중 · ${contentProgress}${contentStage?` · ${contentStage}`:''}`:embedding?`EmbeddingGemma 2 Q8_0 · ${chunkStatus} 분석 중`:phase==='watch'?'변경 파일 확인 중':`파일을 살펴보고 있어요 · ${indexed}개 저장`;const pageProgress=pdf?` · ${index.current_page}/${index.total_pages}페이지`:'';const phaseHint=metadata?' · 파일명 검색을 먼저 준비하고 있어요':content?' · 본문과 OCR은 백그라운드에서 처리 중이에요':embedding?' · 키워드 검색은 계속 사용할 수 있어요':'';dom.progressDetail.textContent=`${index.current_path?`${humanPath(index.current_path)}${pageProgress} · `:''}${scanned}개 파일 확인 · ${errors}개 읽기 건너뜀${phaseHint}`;}
    else if(terminal){const hasReuse=Number.isFinite(Number(index.reused))&&index.reused!==null&&index.reused!==undefined&&Number.isFinite(Number(index.reused_chunks))&&index.reused_chunks!==null&&index.reused_chunks!==undefined;const reused=hasReuse?` · 재사용 ${Number(index.reused)}개 파일·${Number(index.reused_chunks)}개 조각`:'';const hasContent=Number.isFinite(Number(index.content_done))&&index.content_done!==null&&index.content_done!==undefined&&Number.isFinite(Number(index.content_total))&&index.content_total!==null&&index.content_total!==undefined;const contentSummary=hasContent?` · 본문 ${Number(index.content_done)}/${Number(index.content_total)}개 파일`:'';const hasEmbedding=Number.isFinite(Number(index.embedding_done_chunks))&&index.embedding_done_chunks!==null&&index.embedding_done_chunks!==undefined&&Number.isFinite(Number(index.embedding_total_chunks))&&index.embedding_total_chunks!==null&&index.embedding_total_chunks!==undefined;const embeddingSummary=index.embedding_state==='disabled'?' · 의미 색인 꺼짐':index.embedding_state==='pending'?` · 의미 대기 ${hasEmbedding?`${Number(index.embedding_done_chunks)}/${Number(index.embedding_total_chunks)}개 조각`:''}`:index.embedding_state==='complete'&&hasEmbedding?` · 의미 ${Number(index.embedding_done_chunks)}/${Number(index.embedding_total_chunks)}개 조각`:'';const label=phase==='complete'&&index.embedding_state==='disabled'?'파일명·본문 색인 완료 · 의미 색인 꺼짐':phase==='complete'&&index.embedding_state==='pending'?'색인은 끝났지만 의미 색인이 남아 있어요':terminalLabels[phase];dom.progressLabel.textContent=label;dom.progressDetail.textContent=`확인 ${scanned}개 · 목록 갱신 ${indexed}개 · 건너뜀 ${skipped}개 · 오류 ${errors}개${contentSummary}${embeddingSummary}${reused}${index.last_error?` · ${index.last_error}`:''}`;}
    dom.librarySummary.textContent=`파일 ${Number(counts.files)||0}개 · 내용 조각 ${Number(counts.chunks)||0}개`+(ready?` · 벡터 ${Number(counts.vectors)||0}개`:'');
    const hasRoots=state.roots.length>0,hasFolder=Boolean(state.folder),scopeReady=!hasFolder||(Boolean(state.folder.path)&&!state.folder.error&&!state.folder.pending);dom.welcome.hidden=hasRoots||hasFolder;dom.resultsView.hidden=false;if(!dom.query.value.trim())dom.resultsTitle.textContent='검색 결과';
    dom.libraryIdle.hidden=(!hasRoots&&!hasFolder)||Boolean(state.query)||!scopeReady;if((hasRoots||hasFolder)&&!state.query&&scopeReady){const hasFiles=hasFolder?state.folder.indexed:(Number(counts.files)||0)>0;dom.idleTitle.textContent=hasFolder?(hasFiles?'이 폴더를 검색할 준비가 됐어요':'이 폴더에는 아직 색인된 파일이 없어요'):(hasFiles?'라이브러리를 검색할 준비가 됐어요':'검색 위치를 연결했어요');dom.idleMessage.textContent=hasFolder?'아래 표시된 폴더와 하위 폴더만 검색합니다.':hasFiles?`파일 ${Number(counts.files)||0}개가 준비됐어요. 이름이나 내용으로 찾아보세요.`:'색인을 시작하면 파일 이름과 문서 내용을 검색할 수 있어요.';dom.startIndexEmpty.hidden=hasFolder||hasFiles||running;}
    if(state.view==='browse'&&!dom.query.value.trim())updateBrowseTitle();
    renderFolderScope();
    if(index.last_error&&!running&&index.last_error!==state.reportedIndexError){state.reportedIndexError=index.last_error;toast(index.last_error,true);}else if(!index.last_error)state.reportedIndexError='';
    renderOcrStatus(data.ocr||{});renderModelDownload(data.model_download||{});
    if(previousListRevision!==undefined&&data.list_revision!==undefined&&previousListRevision!==data.list_revision&&state.view==='browse'&&!state.folder?.pending&&!state.folder?.error)beginBrowseLoad();
    if(wasIndexRunning&&!running&&state.folder?.path){
      if(state.view==='browse'&&!state.folder.pending&&!state.folder.error){
        const path=state.folder.path,requested=state.folder.requested,sequence=state.scopeSequence;
        window.moa.invoke('scope',{path}).then(folder=>{
          if(!state.closed&&state.view==='browse'&&sequence===state.scopeSequence&&state.folder?.path===path){state.folder={...folder,requested,pending:false};renderFolderScope();}
        }).catch(error=>console.warn('scope refresh',error.message));
      }else setTimeout(resolveFolderScope,0);
    }
    schedulePoll(running||state.modelDownload?.running?2800:12000);
  }
  function renderModelDownload(download={}) {
    state.modelDownload=download;
    const running=Boolean(download.running);for(const button of [dom.downloadEmbedding,dom.downloadChat,dom.downloadOcr])button.disabled=running;const save=dom.settingsForm.querySelector('[type="submit"]');if(save)save.disabled=running;
    dom.downloadProgress.hidden=!running&&!download.error;
    if(running){const received=Number(download.received)||0,total=Number(download.total)||0;const bytes=total?`${fmtBytes(received)} / ${fmtBytes(total)}`:`${fmtBytes(received)} 받음`;const labels={embedding:'EmbeddingGemma 2 Q8_0',chat:'Gemma 4 E2B IT QAT Q4_0',ocr:'PP-OCRv5 OCR'};dom.downloadProgress.classList.add('running');dom.downloadProgressText.textContent=`${labels[download.kind]||'모델'} · ${bytes} · ${download.message||'다운로드 중'}`;if(download.kind==='embedding')dom.settingsForm.elements.namedItem('embedding_model').dataset.downloadKind='embedding';if(download.kind==='chat')dom.settingsForm.elements.namedItem('chat_model').dataset.downloadKind='chat';return;}
    dom.downloadProgress.classList.remove('running');
    if(download.error){dom.downloadProgressText.textContent=download.error;dom.downloadProgress.hidden=false;if(download.error!==state.reportedDownloadError){state.reportedDownloadError=download.error;dom.settingsFeedback.textContent=download.error;}return;}
    if(state.modelDownloadKind){const kind=state.modelDownloadKind;dom.downloadProgress.hidden=true;if(kind==='ocr'){renderOcrStatus(state.status?.ocr||{});dom.settingsFeedback.textContent=state.status?.ocr?.ready?'이미지·스캔 PDF OCR 모델을 설치했어요. 다시 색인하면 내용을 검색할 수 있어요.':'OCR 모델 다운로드가 끝났지만 준비 상태를 확인할 수 없어요.';}else{const key=kind==='chat'?'chat_model':'embedding_model';const input=dom.settingsForm.elements.namedItem(key);const saved=state.status?.settings?.[key]||'';if(input&&!input.value&&saved)input.value=saved;dom.settingsFeedback.textContent=saved?`${kind==='chat'?'Gemma 4 E2B IT QAT Q4_0':'EmbeddingGemma 2 Q8_0'}를 설치하고 경로를 저장했어요.${input?.value===saved?'':' 입력 중인 경로는 유지했어요. 설정 저장을 눌러 경로를 적용하세요.'}`:'모델을 받았어요. 모델 경로를 확인한 뒤 설정을 저장해 주세요.';}state.modelDownloadKind='';}
  }
  function renderOcrStatus(ocr={}) { if(!dom.ocrStatus)return;const ready=Boolean(ocr.ready);dom.ocrStatus.textContent=ocr.message||(ready?'한국어·영어 이미지 및 스캔 PDF OCR 모델 파일이 준비되었어요.':'이미지·PDF OCR 모델이 준비되지 않았어요. 모델을 받은 뒤 색인을 시작해 주세요.');dom.ocrStatus.classList.toggle('ready',ready); }
  function schedulePoll(delay) { if(state.closed)return;clearTimeout(state.pollTimer);state.pollTimer=setTimeout(refreshStatus,delay); }
  async function refreshStatus() { if(state.closed)return;try{const data=await window.moa.invoke('status');if(!state.closed)renderStatus(data);}catch(error){if(state.closed)return;schedulePoll(10000);if(!state.status)renderRoots([]);console.warn('status',error.message);} }
  function hashFolder() { const params=new URLSearchParams(location.hash.replace(/^#/,''));return params.has('folder')?params.get('folder'):null; }
  function renderFolderScope() {
    if(state.closed)return;
    const folder=state.folder;dom.folderScope.hidden=!folder;
    if(!folder){dom.query.disabled=false;dom.query.removeAttribute('aria-describedby');return;}
    dom.folderScopePath.textContent=folder.path||folder.requested||'';dom.folderScopePath.title=folder.path||folder.requested||'';
    const checked=Boolean(folder.path)&&!folder.error&&!folder.pending;const running=Boolean(state.status?.index?.running);dom.query.disabled=!checked;dom.query.title=folder.error||(!checked?'폴더 범위를 확인하고 있어요':'');
    if(folder.pending)dom.folderScopeNote.textContent='폴더 위치를 확인하고 있어요…';
    else if(folder.error)dom.folderScopeNote.textContent=`폴더 범위를 확인하지 못했어요. ${folder.error}`;
    else if(!folder.indexed)dom.folderScopeNote.textContent=(folder.covered?`아직 검색할 파일이 없어 색인이 필요해요. 등록 위치: ${folder.root_path||'상위 폴더'}`:'아직 등록되지 않은 위치예요. 색인을 시작하면 새 검색 위치로 추가합니다.')+' 등록된 다른 위치도 함께 갱신됩니다.';
    else dom.folderScopeNote.textContent='이 폴더와 하위 폴더만 검색합니다. 색인 시작 시 등록된 다른 위치도 함께 갱신됩니다.';
    dom.folderScopeIndex.hidden=!checked;dom.folderScopeIndex.disabled=!checked||running||state.indexingScope;dom.folderScopeIndex.textContent=state.indexingScope?'색인 준비 중…':running?'색인 중…':folder.indexed?'다시 색인':'색인 시작';
    dom.folderScopeClear.disabled=false;
  }
  function currentFolderPath() { return state.folder?.path&&!state.folder.pending&&!state.folder.error?state.folder.path:null; }
  function currentBrowse(generation=state.viewGeneration) { return !state.closed&&state.view==='browse'&&generation===state.viewGeneration&&!dom.query.value.trim()&&!state.folder?.pending&&!state.folder?.error; }
  function updateBrowseTitle() {
    const count=state.browse.items.length,total=state.browse.total;
    dom.resultsTitle.textContent=state.browse.loading?`파일 ${total||count}개 · ${count}개 불러옴 · 더 불러오는 중…`:total?`파일 ${total}개 · ${count}개 불러옴`:state.browse.failed?'파일 목록을 불러오지 못했어요':'파일 0개';
  }
  function browseRetryButton() {
    const button=element('button','secondary-button','다시 불러오기');button.type='button';button.addEventListener('click',()=>{if(!state.browse.failed||state.browse.loading)return;const reset=state.browse.items.length===0;state.browse.failed=false;loadBrowsePage(reset,state.viewGeneration);});return button;
  }
  function createResultCard(result) {
    const card=element('div','result-card'+(state.selected?.id===result.id?' selected':''));
    card.setAttribute('role','option');card.setAttribute('tabindex',state.selected?.id===result.id?'0':'-1');card.setAttribute('aria-selected',String(state.selected?.id===result.id));card.setAttribute('aria-label',`${result.name}, ${result.path}`);card.setAttribute('data-result-id',String(result.id));
    const title=element('span','result-title',result.name);title.title=result.name;
    const path=element('span','result-path',result.path);path.title=result.path;
    const matchSignals=new Set(String(result.match_type||'').split('+'));
    const matchLabel=state.view==='browse'?'—':[ ['filetype','소스코드 파일'],['filename','파일명·경로'],['keyword','본문 키워드'],['semantic','의미 유사'] ].filter(([signal])=>matchSignals.has(signal)).map(([,label])=>label).join(' + ')||'파일명·경로';
    const match=element('span','match-tag',matchLabel);match.title=matchLabel+(result.location?` · ${result.location}`:'');
    card.append(fileIcon(result.extension),title,path,element('span','result-size',fmtBytes(result.size)),element('span','result-date',fmtDate(result.modified)),match);
    card.addEventListener('click',()=>selectResult(result,card,true));
    card.addEventListener('dblclick',()=>fileAction(result.id,'open'));
    card.addEventListener('keydown',event=>handleResultKey(event,false));
    return card;
  }
  function renderBrowseWindow() {
    const list=state.browse.items,rowHeight=state.virtualRowHeight,scrollTop=Number(dom.resultsList.scrollTop)||0;
    const viewportRows=Math.ceil((Number(dom.resultsList.clientHeight)||580)/rowHeight);
    const maxRows=100,visibleRows=Math.max(1,Math.min(maxRows,viewportRows+24));
    const start=Math.max(0,Math.min(list.length,Math.floor(scrollTop/rowHeight)-12));
    const end=Math.min(list.length,start+visibleRows);
    state.virtualStart=start;
    const oldCards=new Map(Array.from(dom.resultsList.querySelectorAll('.result-card')).map(card=>[card.getAttribute('data-result-id'),card]));
    const oldTop=Array.from(dom.resultsList.children).find(child=>child.className.includes('list-spacer-top'));
    const oldBottom=Array.from(dom.resultsList.children).find(child=>child.className.includes('list-spacer-bottom'));
    const nodes=[];
    if(start>0){const spacer=oldTop||element('div','list-spacer list-spacer-top');spacer.style.height=`${start*rowHeight}px`;spacer.setAttribute('aria-hidden','true');nodes.push(spacer);}
    for(let index=start;index<end;index++){const result=list[index];const card=oldCards.get(String(result.id))||createResultCard(result);const selected=String(result.id)===String(state.selected?.id);card.classList.toggle('selected',selected);card.setAttribute('aria-selected',String(selected));card.setAttribute('tabindex',selected?'0':'-1');card.setAttribute('aria-posinset',String(index+1));card.setAttribute('aria-setsize',String(state.browse.total||list.length));nodes.push(card);}
    if(end<list.length){const spacer=oldBottom||element('div','list-spacer list-spacer-bottom');spacer.style.height=`${(list.length-end)*rowHeight}px`;spacer.setAttribute('aria-hidden','true');nodes.push(spacer);}
    if(!list.length){const empty=element('div','results-empty',state.browse.loading?'파일 목록을 불러오는 중…':state.browse.failed?'파일 목록을 불러오지 못했어요':state.folder?'이 폴더와 하위 폴더에 파일이 없습니다':'검색 위치를 추가하면 파일 목록을 볼 수 있습니다.');if(state.browse.failed)empty.append(browseRetryButton());nodes.push(empty);}
    else if(state.browse.failed){const retry=element('div','results-empty','다음 파일 페이지를 불러오지 못했어요.');retry.append(browseRetryButton());nodes.push(retry);}
    dom.resultsList.replaceChildren(...nodes);
    updateBrowseTitle();
  }
  function beginBrowseLoad() {
    if(state.closed)return;
    const query=dom.query.value.trim();
    clearTimeout(state.searchTimer);clearTimeout(state.hybridTimer);
    state.answerSequence++;state.searchSequence++;state.viewGeneration++;state.view='browse';state.query='';state.searchPending=false;state.selectionExplicit=false;state.results=[];state.selected=null;
    dom.answerRequest.disabled=true;dom.answerView.hidden=true;dom.welcome.hidden=state.roots.length>0||Boolean(state.folder);dom.resultsView.hidden=false;dom.libraryIdle.hidden=(!state.roots.length&&!state.folder);renderPreviewPlaceholder();
    state.browse={items:[],total:0,nextCursor:null,revision:null,loading:false,failed:false};dom.resultsList.scrollTop=0;renderBrowseWindow();
    if(query){dom.query.value='';}
    const generation=state.viewGeneration;
    if(state.folder?.pending||state.folder?.error){renderBrowseWindow();return;}
    loadBrowsePage(true,generation);
  }
  async function loadBrowsePage(reset,generation=state.viewGeneration,resetCount=0) {
    if(!currentBrowse(generation)||state.browse.loading||(!reset&&(!state.browse.nextCursor||state.browse.failed)))return;
    const cursor=reset?null:state.browse.nextCursor;
    state.browse.loading=true;state.browse.failed=false;renderBrowseWindow();
    try{
      const data=await window.moa.invoke('list_files',{limit:200,extension:state.extension||null,folder:currentFolderPath(),cursor});
      if(!currentBrowse(generation))return;
      if((!reset&&data.reset_required)||(!reset&&state.browse.revision!==null&&data.revision!==state.browse.revision)){
        state.browse.loading=false;
        if(resetCount>=1){state.browse.failed=true;renderBrowseWindow();return;}
        state.browse={items:[],total:0,nextCursor:null,revision:null,loading:false,failed:false};state.results=[];state.selected=null;state.selectionExplicit=false;dom.resultsList.scrollTop=0;renderPreviewPlaceholder();renderBrowseWindow();
        await loadBrowsePage(true,generation,resetCount+1);return;
      }
      const oldLength=state.browse.items.length;
      const seen=new Set(state.browse.items.map(item=>String(item.id)));
      for(const result of data.results||[])if(!seen.has(String(result.id))){state.browse.items.push(result);seen.add(String(result.id));}
      state.browse.total=Number(data.total)||0;state.browse.nextCursor=data.next_cursor||null;state.browse.revision=data.revision??state.browse.revision;state.browse.failed=false;
      state.results=state.browse.items;
      if(!state.selected&&state.results.length){state.selected=state.results[0];state.selectionExplicit=false;}
      renderBrowseWindow();
      if(oldLength===0&&state.selected)selectResult(state.selected,$('.result-card',dom.resultsList),false);
      else if(state.selected){for(const card of dom.resultsList.querySelectorAll('.result-card')){const item=state.results.find(result=>String(result.id)===card.getAttribute('data-result-id'));if(item)card.setAttribute('aria-selected',String(item.id===state.selected.id));}}
      if(state.browse.nextCursor&&state.results.length<state.browse.total&&oldLength===state.results.length)state.browse.nextCursor=null;
    }catch(error){
      if(!currentBrowse(generation))return;
      state.browse.failed=true;toast(error.message,true);
      renderBrowseWindow();
    }finally{
      if(currentBrowse(generation)){state.browse.loading=false;updateBrowseTitle();}
    }
  }
  function onBrowseScroll() {
    if(state.view!=='browse'||state.browse.loading||!state.browse.nextCursor)return;
    const remaining=dom.resultsList.scrollHeight-dom.resultsList.scrollTop-dom.resultsList.clientHeight;
    if(remaining<Math.max(300,state.virtualRowHeight*12))loadBrowsePage(false,state.viewGeneration);
    else renderBrowseWindow();
  }
  async function resolveFolderScope() {
    if(state.closed)return;state.query=dom.query.value.trim();const requested=hashFolder(),sequence=++state.scopeSequence;state.searchSequence++;state.answerSequence++;state.searchPending=Boolean(state.query);state.results=[];state.selected=null;dom.answerRequest.disabled=true;dom.answerView.hidden=true;clearTimeout(state.searchTimer);
    if(requested===null){state.folder=null;dom.query.disabled=false;renderFolderScope();if(state.query)startSearchView(state.query,false);else beginBrowseLoad();return;}
    state.folder={requested,pending:true};state.browse={items:[],total:0,nextCursor:null,revision:null,loading:false,failed:false};dom.resultsList.replaceChildren();dom.resultsView.hidden=false;renderFolderScope();
    try{const folder=await window.moa.invoke('scope',{path:requested});if(state.closed||sequence!==state.scopeSequence)return;state.folder={...folder,requested,pending:false};renderFolderScope();if(state.query)startSearchView(state.query,false);else beginBrowseLoad();}
    catch(error){if(state.closed||sequence!==state.scopeSequence)return;state.folder={requested,pending:false,error:error.message};state.searchPending=false;state.answerRequest.disabled=true;renderFolderScope();state.browse={items:[],total:0,nextCursor:null,revision:null,loading:false,failed:true};renderBrowseWindow();dom.resultsView.hidden=false;toast(`폴더 범위를 확인하지 못했어요: ${error.message}`,true);}
  }
  async function indexCurrentFolder() {
    const folder=state.folder;if(state.closed||!folder?.path||folder.error||folder.pending||state.indexingScope)return;
    state.indexingScope=true;const sequence=state.scopeSequence;renderFolderScope();
    try{if(!folder.covered){await window.moa.invoke('add_root',{path:folder.path});await refreshStatus();}if(state.closed||sequence!==state.scopeSequence||state.folder?.path!==folder.path)return;await window.moa.invoke('start_index');await refreshStatus();if(!state.closed&&sequence===state.scopeSequence&&state.folder?.path===folder.path){const current=await window.moa.invoke('scope',{path:folder.path});if(!state.closed&&sequence===state.scopeSequence&&state.folder?.path===folder.path){state.folder={...current,requested:folder.requested,pending:false};renderFolderScope();if(state.query)search();}}if(!state.closed)toast('등록된 위치의 색인을 시작했어요. 검색 결과는 선택한 폴더 안으로 제한됩니다.');}
    catch(error){toast(error.message,true);}
    finally{state.indexingScope=false;renderFolderScope();}
  }
  function fillSettings(settings={}) { for(const key of ['embedding_model','chat_model','n_gpu_layers','context_size','scan_interval','max_file_mb','filename_only_dirs','gpu_mode','chat_enabled']){const input=dom.settingsForm.elements.namedItem(key);if(!input||document.activeElement===input||settings[key]===undefined)continue;if(key==='chat_enabled')input.checked=Boolean(settings[key]);else input.value=settings[key];} }
  async function chooseRoot() {
    if(state.addingRoot||state.closed)return;dom.rootAddPath.value='';dom.rootAddFeedback.textContent='';dom.rootPathSuggestions.replaceChildren();for(const suggestion of state.status?.suggestions||[]){const option=document.createElement('option');option.value=suggestion.path;option.label=suggestion.label;dom.rootPathSuggestions.append(option);}dom.rootAddSubmit.disabled=false;dom.rootAddDialog.showModal();dom.rootAddPath.focus();
  }
  async function addRoot(event){event.preventDefault();if(state.addingRoot||state.closed)return;const path=dom.rootAddPath.value.trim();if(!path){dom.rootAddFeedback.textContent='폴더 경로를 입력해 주세요.';dom.rootAddPath.focus();return;}try{state.addingRoot=true;dom.rootAddSubmit.disabled=true;dom.rootAddFeedback.textContent='폴더를 확인하고 있어요…';await window.moa.invoke('add_root',{path});dom.rootAddDialog.close();await refreshStatus();toast('검색 위치를 추가했어요. 지금 색인을 시작할 수 있어요.');}catch(error){dom.rootAddFeedback.textContent=error.message;}finally{state.addingRoot=false;if(!state.closed)dom.rootAddSubmit.disabled=false;}}
  async function startIndex(){try{await window.moa.invoke('start_index');await refreshStatus();toast('색인을 시작했어요.');}catch(error){toast(error.message,true);}}
  async function cancelIndex(){try{await window.moa.invoke('cancel_index');await refreshStatus();toast('색인을 정리하고 있어요.');}catch(error){toast(error.message,true);}}
  async function downloadModel(kind){if(state.modelDownload?.running)return;if(kind==='chat'&&!state.status?.settings?.chat_enabled){dom.settingsFeedback.textContent='Gemma 4 답변을 켜고 설정을 저장한 뒤 다운로드할 수 있어요.';return;}state.modelDownloadKind=kind;state.reportedDownloadError='';const labels={embedding:'EmbeddingGemma 2 Q8_0',chat:'Gemma 4 E2B IT QAT Q4_0',ocr:'PP-OCRv5 OCR'};dom.settingsFeedback.textContent=`${labels[kind]} 다운로드를 요청했어요. 준비 중입니다…`;try{await window.moa.invoke('download_model',{kind});await refreshStatus();toast(`${labels[kind]} 다운로드를 시작했어요.`);}catch(error){state.modelDownloadKind='';dom.settingsFeedback.textContent=error.message;toast(error.message,true);}}
  function isCurrentSearch(query,generation) { return !state.closed&&state.view==='search'&&generation===state.viewGeneration&&dom.query.value.trim()===query&&state.query===query&&!state.folder?.pending&&!state.folder?.error; }
  function applySearchResults(data,pending,phase='') {
    state.searchPending=pending;state.results=data.results||[];const total=state.results.length;
    dom.resultsTitle.textContent=phase?`“${state.query}”의 파일명·키워드 결과 ${total}개 · ${phase}`:total?`“${state.query}”의 결과 ${total}개`:'찾은 파일이 없어요';
    renderResults(phase?{...data,warnings:[...(data.warnings||[]),'파일명·키워드 결과를 먼저 보여드리고 있어요. 의미 검색을 마치면 결과가 갱신됩니다.']}:data);
    dom.answerRequest.disabled=!answerAvailable();
  }
  function startFastTask(task) {
    state.instantInFlight=task;
    task.promise=window.moa.invoke('search',{query:task.query,limit:30,extension:state.extension||null,folder:currentFolderPath(),mode:state.mode==='keyword'?'keyword':'instant'});
    task.promise.then(data=>{
      task.data=data;
      if(!isCurrentSearch(task.query,task.generation))return;
      state.lastInstantData=data;
      const pending=state.mode==='hybrid'&&Boolean(state.status?.engine?.embedding_ready);
      applySearchResults(data,pending,pending?'의미 검색 중':'');
      if(state.mode==='hybrid'&&!pending)dom.searchHint.textContent='파일명·키워드 결과를 먼저 보여드립니다. 의미 검색은 EmbeddingGemma 2 Q8_0 준비 후 사용할 수 있어요.';
    }).catch(error=>{
      if(!isCurrentSearch(task.query,task.generation))return;
      state.searchPending=false;dom.answerRequest.disabled=true;dom.resultsTitle.textContent='검색하지 못했어요';dom.resultsList.replaceChildren(element('div','results-empty',error.message));toast(error.message,true);
    }).finally(()=>{
      if(state.instantInFlight===task)state.instantInFlight=null;
      if(task.generation===state.viewGeneration&&task.query===state.query)state.instantCompleted=task;
      task.resolve?.(task.data||null);
      const queued=state.instantQueued;state.instantQueued=null;
      if(queued&&isCurrentSearch(queued.query,queued.generation))startFastTask(queued);else queued?.resolve?.();
    });
  }
  function requestFastSearch(query,generation) {
    const completed=state.instantCompleted;
    if(completed?.query===query&&completed.generation===generation)return completed.done;
    const active=state.instantInFlight;
    if(active?.query===query&&active.generation===generation)return active.done;
    const queued=state.instantQueued;
    if(queued?.query===query&&queued.generation===generation)return queued.done;
    let resolve;
    const task={query,generation,done:new Promise(done=>{resolve=done;}),resolve:null};task.resolve=resolve;
    if(active){if(queued)queued.resolve();state.instantQueued=task;return task.done;}
    startFastTask(task);return task.done;
  }
  function runHybridSearch(query,generation) {
    const active=state.hybridInFlight;
    if(active?.query===query&&active.generation===generation)return active.promise;
    const task={query,generation,promise:null};state.hybridInFlight=task;
    task.promise=performHybridSearch(query,generation).finally(()=>{if(state.hybridInFlight===task)state.hybridInFlight=null;});
    return task.promise;
  }
  async function performHybridSearch(query,generation) {
    const fastDone=await requestFastSearch(query,generation);
    if(!isCurrentSearch(query,generation)||fastDone===null)return;
    if(!state.status?.engine?.embedding_ready){state.searchPending=false;dom.answerRequest.disabled=!answerAvailable();return;}
    try{
      const hybrid=await window.moa.invoke('search',{query,limit:30,extension:state.extension||null,folder:currentFolderPath(),mode:'hybrid'});
      if(!isCurrentSearch(query,generation))return;
      applySearchResults(hybrid,false);
    }catch(error){
      if(!isCurrentSearch(query,generation))return;
      applySearchResults({...state.lastInstantData,warnings:[...(state.lastInstantData?.warnings||[]),`의미 검색을 마치지 못했어요: ${error.message}`]},false);
    }
  }
  function startSearchView(query=dom.query.value.trim(),hybridDelay=200) {
    if(state.closed)return;
    query=query.trim();if(!query){beginBrowseLoad();return;}if(state.folder?.pending||state.folder?.error){state.searchPending=false;dom.answerRequest.disabled=true;dom.resultsList.replaceChildren(element('div','results-empty','폴더 검색 범위를 확인할 수 없습니다.'));return;}
    clearTimeout(state.searchTimer);clearTimeout(state.hybridTimer);
    state.view='search';state.query=query;const generation=++state.viewGeneration;state.instantCompleted=null;state.searchSequence++;
    state.answerSequence++;state.searchPending=true;state.selectionExplicit=false;state.results=[];state.selected=null;
    dom.answerRequest.disabled=true;dom.answerView.hidden=true;dom.welcome.hidden=true;dom.libraryIdle.hidden=true;dom.resultsView.hidden=false;dom.resultsTitle.textContent='파일 찾는 중…';dom.resultsList.replaceChildren(element('div','results-empty','파일명과 본문을 찾고 있어요…'));renderPreviewPlaceholder();updateSearchHint();
    requestFastSearch(query,generation);
    if(state.mode==='hybrid'&&state.status?.engine?.embedding_ready){if(hybridDelay<=0)runHybridSearch(query,generation);else state.hybridTimer=setTimeout(()=>runHybridSearch(query,generation),hybridDelay);}
  }
  function renderResults(data={}) {
    dom.resultsList.replaceChildren();dom.answerView.hidden=true;
    if(data.intent==='source_code')dom.searchHint.textContent='소스코드 파일과 검색어가 포함된 문서를 찾습니다.';else updateSearchHint();
    if(data.warnings?.length)dom.resultsList.append(element('div','search-hint-inline',data.warnings.join(' · ')));
    if(!state.results.length){const empty=element('div','results-empty',state.folder?'이 폴더와 하위 폴더에서 찾지 못했어요. 다른 검색어로 시도해 보세요.':'다른 단어로 다시 검색하거나 검색 위치를 추가해 보세요.');dom.resultsList.append(empty);state.selected=null;renderPreviewPlaceholder();return;}
    for(const result of state.results)dom.resultsList.append(createResultCard(result));
    if(!state.selectionExplicit||!state.selected||!state.results.some(r=>r.id===state.selected.id))selectResult(state.results[0],$('.result-card',dom.resultsList),false);
  }
  function clearForComposition() {
    clearTimeout(state.searchTimer);clearTimeout(state.hybridTimer);state.viewGeneration++;state.instantCompleted=null;state.searchSequence++;state.answerSequence++;state.view='search';state.query='';state.searchPending=true;state.results=[];state.selected=null;state.selectionExplicit=false;state.instantQueued?.resolve?.();state.instantQueued=null;
    dom.answerRequest.disabled=true;dom.answerView.hidden=true;dom.resultsView.hidden=false;dom.resultsTitle.textContent='한글 입력 중…';dom.resultsList.replaceChildren(element('div','results-empty','입력이 끝나면 검색합니다…'));renderPreviewPlaceholder();
  }
  function handleQueryInput(event={}) {
    if(event.isComposing||state.composing){
      if(!state.composing){state.composing=true;clearForComposition();}
      return;
    }
    const query=dom.query.value.trim();
    if(query===state.query&&((query&&state.view==='search')||(!query&&state.view==='browse')))return;
    if(query)startSearchView(query,200);else beginBrowseLoad();
  }
  function renderPreviewPlaceholder(message='파일을 선택해 주세요') {dom.preview.replaceChildren();const empty=element('div','preview-empty');const icon=element('span','preview-empty-icon');icon.innerHTML='<svg viewBox="0 0 32 32" aria-hidden="true"><path d="M8 4.5h10l7 7v16H8z"/><path d="M18 4.5v7h7M12 17h9M12 21h9"/></svg>';empty.append(icon,element('strong','',message),element('span','',message==='파일을 선택해 주세요'?'내용과 위치를 여기서 확인할 수 있어요.':''));dom.preview.append(empty);}
  async function selectResult(result,card,explicit=true) {
    state.selectionExplicit=explicit;
    const sequence=state.previewSequence=(state.previewSequence||0)+1;
    state.selected=result;for(const item of dom.resultsList.querySelectorAll('.result-card')){item.classList.toggle('selected',item===card);item.setAttribute('aria-selected',String(item===card));item.setAttribute('tabindex',item===card?'0':'-1');}
    dom.preview.replaceChildren(element('div','preview-spinner','파일 내용을 불러오는 중…'));
    try{const file=await window.moa.invoke('file',{id:result.id});if(sequence!==state.previewSequence||state.selected?.id!==result.id)return;renderPreview(file);}
    catch(error){if(sequence===state.previewSequence&&state.selected?.id===result.id){renderPreviewPlaceholder('파일 정보를 불러오지 못했어요');dom.preview.append(element('div','preview-error',error.message));}}
  }
  function renderPreview(file) {
    dom.preview.replaceChildren();const head=element('div','preview-file-head');head.append(fileIcon(file.extension));const info=element('div','result-body');info.append(element('div','preview-file-title',file.name),element('div','preview-file-path',file.path));head.append(info);dom.preview.append(head);
    const metadata=element('div','preview-meta');metadata.append(element('span','',fmtBytes(file.size)),element('span','',fmtDate(file.modified)),element('span','',file.extension?`.${String(file.extension).replace(/^\./,'')}`:'파일'));dom.preview.append(metadata);
    const actions=element('div','preview-actions');const open=element('button','secondary-button','파일 열기');open.type='button';open.addEventListener('click',()=>fileAction(file.id,'open'));const reveal=element('button','secondary-button','폴더에서 보기');reveal.type='button';reveal.addEventListener('click',()=>fileAction(file.id,'reveal'));actions.append(open,reveal);dom.preview.append(actions);
    if(file.error)dom.preview.append(element('div','preview-error',file.error));
    if(file.passages?.length){dom.preview.append(element('div','preview-section-title',`파일 내용 · ${file.passages.length}개 구간`));const list=element('div','passage-list');for(const passage of file.passages){const card=element('div','passage-card');if(passage.location)card.append(element('div','passage-location',passage.location));card.append(element('div','passage-text',passage.text));list.append(card);}dom.preview.append(list);}
    else if(!file.error&&String(file.reader_id||'').startsWith('filename-only-v1:'))dom.preview.append(element('div','filename-only-note','설정에 따라 파일명·경로만 색인했어요.'));
    else if(!file.error)dom.preview.append(element('div','preview-spinner','내용이 없거나 색인이 아직 진행 중이에요.'));
  }
  async function fileAction(id,action){try{await window.moa.invoke(action==='open'?'open_file':'reveal_file',{id});}catch(error){toast(error.message,true);}}
  async function handleResultKey(event,fromQuery) {
    if(event.isComposing||state.composing||event.keyCode===229||!state.results.length||dom.query.value.trim()!==state.query)return;
    const current=state.results.findIndex(result=>String(result.id)===String(state.selected?.id));
    let next=current;
    if(event.key==='ArrowDown')next=Math.min(state.results.length-1,current+1);
    else if(event.key==='ArrowUp')next=Math.max(0,current<0?0:current-1);
    else if(!fromQuery&&event.key==='Home')next=0;
    else if(!fromQuery&&event.key==='End')next=state.results.length-1;
    else if(event.key==='Enter'&&state.selected){event.preventDefault();fileAction(state.selected.id,event.ctrlKey?'reveal':'open');return;}
    else return;
    event.preventDefault();
    if(next>=state.results.length-1&&next===current&&event.key==='ArrowDown'&&state.view==='browse'&&state.browse.nextCursor){const generation=state.viewGeneration,selectedId=String(state.selected?.id??'');await loadBrowsePage(false,generation);if(generation!==state.viewGeneration||state.view!=='browse'||String(state.selected?.id??'')!==selectedId||dom.query.value.trim()!==state.query)return;next=Math.min(state.results.length-1,current+1);}
    const result=state.results[next];if(!result)return;
    if(state.view==='browse'){
      const rowTop=next*state.virtualRowHeight,rowBottom=rowTop+state.virtualRowHeight,viewTop=dom.resultsList.scrollTop||0,viewBottom=viewTop+(dom.resultsList.clientHeight||580);
      if(rowTop<viewTop)dom.resultsList.scrollTop=rowTop;else if(rowBottom>viewBottom)dom.resultsList.scrollTop=rowBottom-(dom.resultsList.clientHeight||580);
      renderBrowseWindow();
    }
    const card=Array.from(dom.resultsList.querySelectorAll('.result-card')).find(item=>item.getAttribute('data-result-id')===String(result.id));
    selectResult(result,card);card?.scrollIntoView({block:'nearest'});if(!fromQuery)card?.focus();
  }
  async function askAnswer(){const query=dom.query.value.trim();if(state.closed||state.view!=='search'||!query||query!==state.query||!state.status?.settings?.chat_enabled||!state.status?.engine?.chat_ready||state.searchPending||!state.results.length||state.folder&&(!state.folder.path||state.folder.pending||state.folder.error))return;const sequence=++state.answerSequence,scopeSequence=state.scopeSequence,folder=state.folder?.path||null;dom.answerView.hidden=false;dom.answerText.textContent='내 파일에서 근거를 살펴보고 있어요…';dom.answerSources.replaceChildren();dom.answerState.textContent='Gemma 4 E2B IT QAT Q4_0 · 생성 중';dom.answerView.scrollIntoView({behavior:'smooth',block:'nearest'});try{const data=await window.moa.invoke('answer',{query,limit:5,folder});if(state.closed||sequence!==state.answerSequence||scopeSequence!==state.scopeSequence||state.query!==query||dom.query.value.trim()!==query)return;dom.answerText.textContent=data.answer||'검색된 내용에서 답변을 만들 수 없었어요.';dom.answerState.textContent='Gemma 4 E2B IT QAT Q4_0';dom.answerSources.replaceChildren();for(const source of data.sources||[]){const chip=element('button','source-chip',`${source.name}${source.location?` · ${source.location}`:''}`);chip.type='button';chip.title=source.path;chip.addEventListener('click',()=>{const result=state.results.find(r=>String(r.id)===String(source.id));if(result){dom.preview.scrollIntoView({behavior:'smooth',block:'center'});selectResult(result,Array.from(dom.resultsList.querySelectorAll('.result-card')).find((_,i)=>state.results[i]?.id===result.id));}});dom.answerSources.append(chip);}for(const warning of data.warnings||[])dom.answerSources.append(element('span','source-chip',warning));}
    catch(error){if(state.closed||sequence!==state.answerSequence||scopeSequence!==state.scopeSequence||state.query!==query)return;dom.answerText.textContent=error.message;dom.answerState.textContent='모델 확인 필요';}}
  function submitSearch(){if(state.composing)return;const query=dom.query.value.trim();if(!query){if(state.view!=='browse')beginBrowseLoad();return;}if(state.view==='search'&&query===state.query){clearTimeout(state.hybridTimer);if(state.mode==='hybrid')runHybridSearch(query,state.viewGeneration);return;}startSearchView(query,0);}
  dom.addRoot.addEventListener('click',chooseRoot);dom.addFolderWelcome.addEventListener('click',chooseRoot);dom.rootAddForm.addEventListener('submit',addRoot);dom.rootAddCancel.addEventListener('click',()=>dom.rootAddDialog.close());dom.rootAddDialog.addEventListener('cancel',()=>{if(!state.addingRoot)dom.rootAddSubmit.disabled=false;});dom.rescanTop.addEventListener('click',startIndex);dom.startIndexEmpty.addEventListener('click',startIndex);dom.cancelIndex.addEventListener('click',cancelIndex);dom.folderScopeIndex.addEventListener('click',indexCurrentFolder);dom.folderScopeClear.addEventListener('click',()=>{history.replaceState(null,'',`${location.pathname}${location.search}`);resolveFolderScope();});dom.openSettings.addEventListener('click',()=>{fillSettings(state.status?.settings||{});dom.settingsFeedback.textContent='';dom.settingsDialog.showModal();});
  dom.searchForm.addEventListener('submit',event=>{event.preventDefault();submitSearch();});
  dom.query.addEventListener('compositionstart',()=>{if(!state.composing){state.composing=true;clearForComposition();}});
  dom.query.addEventListener('compositionend',()=>{state.composing=false;handleQueryInput({isComposing:false});});
  dom.query.addEventListener('input',event=>handleQueryInput(event));
  dom.resultsList.addEventListener('scroll',onBrowseScroll);
  updateSearchHint();document.querySelectorAll('.mode-button').forEach(button=>button.addEventListener('click',()=>{state.mode=button.dataset.mode;document.querySelectorAll('.mode-button').forEach(item=>{const active=item===button;item.classList.toggle('active',active);item.setAttribute('aria-pressed',String(active));});updateSearchHint();if(state.query)startSearchView(state.query,200);}));
  dom.extensionFilter.addEventListener('change',()=>{state.extension=dom.extensionFilter.value;if(state.query)startSearchView(state.query,200);else beginBrowseLoad();});dom.answerRequest.addEventListener('click',askAnswer);
  dom.query.addEventListener('keydown',event=>handleResultKey(event,true));
  window.addEventListener('resize',()=>{if(state.view==='browse')renderBrowseWindow();});
  dom.settingsForm.addEventListener('submit',async event=>{event.preventDefault();const form=new FormData(dom.settingsForm);const settings={embedding_model:String(form.get('embedding_model')||'').trim(),chat_model:String(form.get('chat_model')||'').trim(),chat_enabled:form.get('chat_enabled')==='true',gpu_mode:String(form.get('gpu_mode')||'auto'),n_gpu_layers:Number(form.get('n_gpu_layers')),context_size:Number(form.get('context_size')),scan_interval:Number(form.get('scan_interval')),max_file_mb:Number(form.get('max_file_mb')),filename_only_dirs:String(form.get('filename_only_dirs')||'')};const save=dom.settingsForm.querySelector('[type="submit"]');save.disabled=true;dom.settingsFeedback.textContent='저장하고 있어요…';try{await window.moa.invoke('update_settings',{settings});await refreshStatus();dom.settingsFeedback.textContent='설정을 저장했어요.';}catch(error){dom.settingsFeedback.textContent=error.message;}finally{save.disabled=Boolean(state.modelDownload?.running);}});
  dom.downloadEmbedding.addEventListener('click',()=>downloadModel('embedding'));dom.downloadChat.addEventListener('click',()=>downloadModel('chat'));dom.downloadOcr.addEventListener('click',()=>downloadModel('ocr'));
  document.addEventListener('keydown',event=>{if(dom.settingsDialog.open||dom.rootAddDialog.open)return;if(event.key==='F3'||(event.ctrlKey||event.metaKey)&&['f','k'].includes(event.key.toLowerCase())){event.preventDefault();dom.query.focus();dom.query.select();}});
  window.addEventListener('hashchange',resolveFolderScope);
  resolveFolderScope();refreshStatus();dom.query.focus();
})();
