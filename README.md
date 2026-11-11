# 모아검색

내 PC의 파일 이름과 문서 내용을 한국어로 검색하는 Windows용 로컬 앱입니다. 검색 폴더를 직접 선택하면 내용을 SQLite 키워드 색인과 로컬 임베딩 벡터로 저장하고, 문장을 입력해 관련 파일을 찾습니다. 검색 결과를 바탕으로 답변을 만들고 출처 문서의 위치도 확인할 수 있습니다.

**Ollama는 사용하지 않습니다.** `llama-cpp-python`이 앱 프로세스 안에서 llama.cpp를 실행하고, 앱이 GGUF 모델을 직접 엽니다. 별도 AI 서버나 데몬을 설치할 필요가 없습니다. 검색 화면을 제공하는 루프백 HTTP 서버는 앱이 함께 실행합니다. 모델과 Python 패키지를 설치하고 나면 검색과 추론은 인터넷 연결 없이 PC에서 처리합니다. 이전 설치를 업데이트할 때는 `uv sync --extra ai --extra test --locked`를 실행한 다음 앱을 다시 시작하고 색인하면, 기존 HWP/이미지 메타데이터도 본문을 읽어 갱신합니다.

## 시작하기

필요한 것은 64비트 Windows 10/11과 인터넷 연결입니다. `uv`가 Python 3.11–3.13 및 프로젝트 의존성을 가상 환경에 준비합니다. Windows에서는 PowerShell에서 다음 명령으로 `uv`를 설치할 수 있습니다. 자세한 설치 방법은 [uv 공식 설치 안내](https://docs.astral.sh/uv/getting-started/installation/)를 참고하세요.

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"
```

저장소 폴더에서 엔진과 임베딩·답변 모델, 이미지 OCR 모델을 설치합니다.

```powershell
.\setup-ai.ps1
```

설치가 끝나면 `doctor` 결과가 표시됩니다. 임베딩 모델과 답변 모델은 각각 약 639 MB이고, 한국어 OCR 모델은 두 파일 합계 약 18.3 MB입니다. 앱과 세 모델을 처음 준비할 때 약 1.3 GB를 받습니다. 모델은 각 공식 출처에서 HTTPS로 받고 SHA-256을 확인한 뒤 설치합니다. GGUF 모델은 파일 서명도 확인합니다. 기존에 같은 이름의 파일이 있으면 덮어쓰지 않습니다.

앱을 엽니다.

```powershell
.\start.cmd
```

브라우저에서 `http://127.0.0.1:8765`를 엽니다. PowerShell에서는 `.\start.ps1`, 브라우저 자동 열기를 원하지 않을 때는 `.\start.ps1 -NoBrowser`를 사용할 수 있습니다. 앱은 루프백 주소에서만 대기합니다.

## 사용 방법

1. **폴더 추가**를 누르고 화면에 안내되는 경로를 직접 입력합니다. 추천 경로 번호를 입력해도 됩니다. 폴더를 추가하기 전에는 파일을 읽거나 검색하지 않습니다. 드라이브 전체를 찾고 싶으면 `C:\`처럼 해당 드라이브를 명시적으로 등록합니다.
2. **색인 시작** 또는 **다시 색인**을 누릅니다. 변경된 파일을 확인하고 위치와 파일명을 저장합니다. 설정한 크기 이하의 지원 문서는 본문을 추출하며, 임베딩 모델이 설정되어 있으면 의미 벡터도 만듭니다. 첫 전체 색인은 파일 수와 디스크 성능에 따라 시간이 걸립니다. 진행 중에는 현재 파일, 건너뛴 파일 수, 취소 버튼을 볼 수 있습니다.
3. 자연어 검색창에 “지난달에 받은 영수증”처럼 뜻을 적습니다. **의미 + 키워드**는 모델 벡터와 문자열 검색 결과를 함께 사용합니다. 모델을 설정하지 않았으면 키워드로 검색합니다. **키워드만**은 의미 벡터를 사용하지 않습니다. 파일 종류 필터로 확장자를 좁히고 결과를 눌러 문서의 일치한 내용을 미리 볼 수 있습니다.
4. **내 파일에 질문**을 누르면 검색 결과의 문서 구간을 근거로 답변을 생성합니다. 답변과 함께 출처 파일을 표시합니다. 답변 생성에는 별도의 답변용 모델이 필요합니다. 임베딩·답변·이미지 OCR 모델은 설정에서 각각 받을 수 있습니다. OCR 모델은 CPU에서 한국어·영어 이미지 글자를 읽으며, 내려받은 뒤 오프라인으로 동작합니다.

설정의 **자동 색인 간격**은 변경 파일을 다시 확인하는 주기이며 기본값은 300초입니다. 0으로 설정하면 자동 확인을 끕니다. 파일 크기 기본 한도는 32 MiB입니다. CPU 실행이 기본값입니다. 설정의 GPU 오프로딩 층은 설치된 llama.cpp 런타임이 GPU를 지원할 때만 효과가 있습니다.

## 색인 범위와 개인정보

모아검색은 사용자가 등록한 위치에서 일반 파일의 이름·경로·크기·수정 시각을 저장하고, 지원 문서는 파일당 최대 200,000자까지 읽습니다. PDF는 최대 250페이지까지 읽고 페이지 위치를 보존합니다. 모든 형식의 파일 크기는 기본 32 MiB이며, 파일 크기 한도는 설정에서 최대 512 MiB까지 올릴 수 있습니다. PDF는 별도로 100 MiB, 문서 내부 압축 데이터는 전체 128 MiB, 압축 항목별 16 MiB의 상한도 적용합니다.

아래 형식은 본문을 읽습니다.

- 텍스트와 데이터: `.txt`, `.text`, `.md`, `.markdown`, `.rst`, `.log`, `.csv`, `.tsv`, `.json`, `.jsonl`, `.xml`, `.yaml`, `.yml`, `.toml`, `.ini`, `.cfg`, `.conf`
- 웹·자막·코드: `.html`, `.htm`, `.css`, `.js`, `.jsx`, `.ts`, `.tsx`, `.py`, `.pyi`, `.ipynb`, `.java`, `.kt`, `.kts`, `.scala`, `.go`, `.rs`, `.c`, `.h`, `.cc`, `.cpp`, `.hpp`, `.cs`, `.fs`, `.vb`, `.php`, `.rb`, `.swift`, `.m`, `.mm`, `.sh`, `.bash`, `.ps1`, `.bat`, `.cmd`, `.sql`, `.graphql`, `.proto`, `.dockerfile`, `.make`, `.gradle`, `.properties`, `.tex`, `.bib`, `.srt`, `.vtt`
- 문서: 텍스트 레이어가 있는 `.pdf`, `.docx`, `.xlsx`, `.pptx`, `.odt`, `.ods`, `.odp`, HWP5 `.hwp`, `.hwpx`; 이미지 OCR `.png`, `.jpg`, `.jpeg`, `.bmp`, `.tif`, `.tiff`, `.webp`

이미지는 별도로 받은 RapidOCR PP-OCRv5 모델로 CPU에서 한국어·영어 OCR을 합니다. 이미지 OCR은 파일당 최대 100 MiB, 프레임당 4천만 픽셀(긴 변은 4096픽셀로 축소), 최대 16프레임까지 처리합니다. 전체 파일 한도(기본 32 MiB)가 먼저 적용되므로 기본 설정에서는 32 MiB를 넘는 이미지가 색인되지 않습니다. 스캔 PDF OCR, 오디오 전사·검색, 동영상 분석, 구형 HWP3와 `.doc`는 지원하지 않아 파일명과 경로로만 검색합니다. HWP5/HWPX라도 암호화되었거나 배포용 문서 등 보호된 본문은 읽지 못할 수 있습니다. 이미지의 손글씨·도형 안 글자 인식은 보장하지 않습니다.

루트 폴더를 직접 고르기 전에는 전체 PC를 훑지 않습니다. 색인은 기본적으로 `.git`, `.svn`, `.hg`, `.ssh`, `.aws`, `.azure`, `.gnupg`, `.codex`, `node_modules`, 가상 환경, 캐시, Windows 및 프로그램·시스템 데이터 폴더와 자격 증명으로 식별되는 파일을 제외합니다. 심볼릭 링크·junction 등 재분석 지점은 따라가지 않고, 온라인 전용 클라우드 자리표시자도 실제 내용을 내려받거나 읽지 않습니다. 등록 폴더 밖의 파일도 읽지 않습니다.

원본 파일은 이동·수정하지 않습니다. 색인, 설정, 임베딩, 다운로드한 모델은 기본적으로 `%LOCALAPPDATA%\MoaSearch`에 저장됩니다. `MOASEARCH_DATA_DIR` 환경 변수나 명령행 `--data-dir`로 저장 위치를 바꿀 수 있습니다. 문서 내용은 이 PC에 설치한 내장 모델로 처리하며, 검색이나 AI 답변에 파일 내용 또는 검색어를 외부로 보내지 않습니다. 설치와 최초 모델 다운로드에 인터넷이 필요합니다. 다운로드 후 앱은 모델·의존성을 위한 추가 인터넷 요청, 텔레메트리, 계정 연결 또는 업데이트 확인을 하지 않습니다.

## 명령행

앱이 설치된 저장소 폴더에서 PowerShell을 사용합니다.

```powershell
# 모델 설치 여부와 현재 색인 상태 확인
.\.venv\Scripts\python.exe -m moasearch doctor

# 내장 기본 모델 확인
.\.venv\Scripts\python.exe -m moasearch models list

# 모델 한 개씩 내려받기 (embedding, chat 또는 ocr)
.\.venv\Scripts\python.exe -m moasearch models download embedding
# 답변 모델
.\.venv\Scripts\python.exe -m moasearch models download chat
# 이미지 OCR 모델
.\.venv\Scripts\python.exe -m moasearch models download ocr

# 지정한 경로만 색인
.\.venv\Scripts\python.exe -m moasearch index "$env:USERPROFILE\Documents"

# 터미널에서 검색
.\.venv\Scripts\python.exe -m moasearch search "지난 분기 회의 일정"
.\.venv\Scripts\python.exe -m moasearch search --keyword "invoice-2026-031"

# 저장 경로를 지정하고 앱 실행
.\.venv\Scripts\python.exe -m moasearch --data-dir "D:\MoaSearchData" serve
```

`--data-dir`는 하위 명령보다 앞에 둡니다. `doctor`, `search`, `index`, `models`에서 같은 방법으로 사용할 수 있습니다. 설치된 런타임이 없을 때 의미 검색과 답변은 사용할 수 없지만, 내장 키워드 검색은 계속 동작합니다.

## 개발 및 검증

개발 환경과 테스트 의존성을 함께 설치하고 단위·API 테스트를 실행합니다.

```powershell
uv sync --extra ai --extra test --locked
.\.venv\Scripts\python.exe -m unittest discover -s tests -v
```

테스트는 임시 폴더와 가짜 모델 응답을 사용하므로 사용자의 문서 폴더나 실제 대형 모델을 색인하지 않습니다. 설계와 검색 파이프라인은 [개발 설계](docs/architecture.md)를 참고하세요.

실제 모델의 의미 검색과 답변을 검증하려면 별도의 데이터 폴더를 사용합니다. 아래 검증은 저장소의 가상 예제 문서 세 개만 색인합니다.

```powershell
.\.venv\Scripts\python.exe -m moasearch --data-dir .moasearch-dev models download embedding
.\.venv\Scripts\python.exe -m moasearch --data-dir .moasearch-dev models download chat
.\.venv\Scripts\python.exe scripts/smoke_ai.py --data-dir .moasearch-dev
.\start.ps1 -DataDir .moasearch-dev
```

현재 단위·API 테스트 78개와 실제 Qwen 모델의 한글 의미 검색, 출처를 포함한 답변 생성을 검증했습니다. 실제 ONNX OCR과 생성한 HWP5/HWPX 파일의 본문·표 추출, 키워드·의미 검색도 확인했습니다. OCR 검증은 네트워크 요청을 차단하고 가상 한글 영수증 이미지를 사용했습니다. 예제 문서 검증 결과이며 대규모 개인 파일 모음의 정확도·성능 측정은 별도로 필요합니다.

## 제한 사항

한글을 포함한 여러 언어를 위한 의미 임베딩과 키워드 검색을 결합하고, 별도 OCR 모델을 내려받으면 지원 이미지의 한국어·영어 인식도 수행하지만, 검색 품질과 응답 정확도를 보장하지는 않습니다. 답변은 검색된 구간에 한정해 근거를 인용하며, 원문이 불완전하거나 색인에서 빠지면 내용을 찾지 못할 수 있습니다. 서로 다른 임베딩 모델의 벡터는 혼합하지 않으며, 모델을 바꾼 뒤 기존 파일의 의미 벡터를 갱신하려면 다시 색인하세요.

벡터 검색은 SQLite에 저장된 벡터를 블록으로 읽어 비교합니다. 일반적인 개인 라이브러리를 목표로 한 초기 구조이며 백만 단위 벡터의 검색 속도나 저장 용량은 검증·보장하지 않습니다.

## 사용 모델 및 출처

- [Qwen3-Embedding-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF), Apache-2.0. Q8_0 약 639 MB, 고정 SHA-256: `06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439`.
- [Qwen3-0.6B-GGUF](https://huggingface.co/Qwen/Qwen3-0.6B-GGUF), Apache-2.0. Q8_0 약 639 MB, 고정 SHA-256: `9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031`.
- [RapidOCR](https://github.com/RapidAI/RapidOCR), Apache-2.0. 한국어 PP-OCRv5 인식 모델과 PP-OCRv5 모바일 검출 모델을 사용하며 SHA-256을 고정해 확인합니다.
- [llama-cpp-python](https://github.com/abetlen/llama-cpp-python)은 llama.cpp의 Python 바인딩입니다. 앱에서 엔진을 직접 로드해 임베딩·대화 추론을 수행합니다.
- 파일 내용 기반 검색의 흐름은 [DigUp](https://github.com/ARahim3/DigUp)의 로컬 추출·벡터 및 키워드 인덱싱 아이디어를 참고했습니다. MoaSearch는 별도 코드로 Windows·한국어 사용에 맞게 작성했습니다.
