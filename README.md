# 모아검색

![모아검색 — 내 파일을 위한 검색](cover.png)

내 PC의 파일 이름과 문서 내용을 한국어로 검색하는 Windows용 **Rust 앱**입니다. 선택한 폴더를 SQLite에 색인하고, Google EmbeddingGemma 2로 자연어 검색하며, Google Gemma 4 E2B로 검색한 문서를 근거로 답변합니다.

Windows 설치형 데스크톱 앱으로, 실행하면 **모아검색 전용 창**이 열립니다. 화면은 앱에 포함된 리소스를 WebView2가 표시하며, 브라우저나 로컬 HTTP 서버를 실행하지 않습니다. 화면은 앱 내부 명령 통로로 Rust 검색 기능을 호출합니다. Python·Ollama·별도 AI 서버는 필요하지 않습니다. Rust 실행 파일에 연결한 llama.cpp가 모델을 앱 안에서 실행합니다. 모델 설치 후 문서 처리·검색·추론은 인터넷 없이 동작합니다. 모델을 새로 받을 때는 인터넷이 필요합니다.

## 설치와 모아런처 등록

[최신 GitHub Release](https://github.com/hebory/moasearch/releases/latest)에서 Windows x64 설치 파일(`*-setup.exe`) 또는 포터블 ZIP을 받을 수 있습니다. Python·Rust 개발 도구 없이 실행하며, 필요한 네이티브 런타임을 패키지에 포함합니다. 설치 파일과 앱 데이터는 분리하고, 설치 제거 시 색인과 모델을 보존합니다.

모아런처에서는 Release의 **`moalauncher.json`**을 내려받은 뒤 **설정 → 카탈로그 가져오기**에서 선택하세요. `hebory/moasearch` 저장소, 설치 파일 패턴과 아이콘·커버 주소가 함께 등록됩니다. JSON을 가져오는 것만으로 설치되지는 않으며, 추가한 모아검색 카드에서 설치를 누릅니다.

처음 실행하면 앱의 **AI 설정**에서 Google 임베딩·답변 모델과 OCR 모델을 준비하세요. 모델 합계 약 3.68 GB는 설치 패키지에 포함하지 않습니다. 모델 없이도 파일명·일반 문서 키워드 검색을 사용할 수 있습니다.

등록 형식과 패키지 재현 방법은 [릴리스 문서](docs/releasing.md), 이미지 원본과 생성 프롬프트는 [이미지 문서](docs/artwork.md)에 있습니다.

## 탐색기에서 현재 폴더 검색

설치할 때 **탐색기에서 현재 폴더 검색** 옵션을 켜면, 폴더 또는 폴더 안의 빈 공간을 우클릭해 **모아검색으로 여기서 검색**을 선택할 수 있습니다. Windows 11에서는 **추가 옵션 표시**에 나타날 수 있습니다. 포터블 ZIP은 메뉴를 자동 등록하지 않습니다.

검색 화면에 선택한 폴더가 표시되고, 파일명·본문·자연어 검색과 AI 답변의 근거가 해당 폴더와 하위 폴더로 제한됩니다. 범위는 앱 창마다 유지하며 **범위 해제**를 눌러 등록된 전체 위치로 돌아갑니다. 경로가 잘못되거나 삭제되면 오류를 표시하고 검색을 중단합니다.

아직 색인하지 않은 폴더는 **색인 시작**을 누르세요. 등록되지 않은 위치는 검색 위치에 추가하며, 기존에 등록된 다른 위치도 함께 갱신합니다. 검색 결과는 선택한 범위로 유지합니다. 상위 폴더가 이미 등록되어 있으면 중복 등록하지 않습니다.

앱이 꺼져 있으면 전용 창을 열고, 같은 Windows 사용자와 데이터 위치로 실행 중인 앱이 있으면 재사용합니다. 탐색기와 앱 사이의 폴더 열기 요청에만 Windows 명명된 파이프를 사용합니다. 같은 폴더 창은 다시 활성화하고 다른 폴더는 새 창에서 검색합니다. 마지막 창을 닫거나 **앱 종료**를 누르면 색인과 모델 다운로드를 정리하고 앱을 종료합니다. 설치 옵션을 해제해 다시 설치하거나 앱을 제거하면 탐색기 메뉴도 제거됩니다.

```powershell
.\dist\moasearch.exe open-folder --path 'D:\문서\계약' | Out-Host
.\dist\moasearch.exe search '보증금 반환 조건' --folder 'D:\문서\계약' | Out-Host
.\dist\moasearch.exe answer '반환 기한은 언제인가요?' --folder 'D:\문서\계약' | Out-Host
.\dist\moasearch.exe instance | Out-Host
.\dist\moasearch.exe quit | Out-Host
```

`open-folder`는 앱 창을 열거나 실행 중인 창에 폴더를 전달합니다. 앱 창과 내부 검색 기능은 별도 포트 설정 없이 함께 실행됩니다.

## 소스에서 실행

이미 빌드된 `dist\moasearch.exe`가 있으면 `start.cmd`를 실행합니다. 실행 파일은 화면 리소스와 SQLite·llama.cpp를 포함합니다. OCR은 Rust OAR/ONNX Runtime, PDF 렌더링은 Windows 기본 기능을 사용합니다. 실행 파일 옆의 ONNX Runtime DLL도 함께 유지하세요.

소스에서 처음 빌드하려면 다음 도구가 필요합니다.

- 64비트 Windows 10/11, Rust MSVC 도구 체인
- Visual Studio 2022 C++ 빌드 도구와 Windows SDK
- CMake, Ninja, LLVM의 `libclang.dll` — 개발 시 바인딩 생성에만 필요

```powershell
.\build.ps1
.\setup-ai.ps1
.\start.cmd
```

`setup-ai.ps1`은 Google 임베딩·답변 모델과 OCR 모델을 다운로드합니다. 합계 약 3.68 GB이며, 고정된 버전 URL·파일 크기·SHA-256을 검증한 뒤 설치합니다. Google 모델은 GGUF 서명도 확인합니다. 같은 이름의 기존 파일을 덮어쓰지 않습니다. 한국어·영어 OCR은 이전 버전과 같은 PP-OCRv5 전용 ONNX 모델 두 개(약 18.3 MB)를 Rust에서 실행합니다. `doctor`와 앱 설정에서 설치 상태를 확인합니다. Windows OCR 언어 팩이나 MSIX 등록은 필요하지 않습니다.

빌드 스크립트는 Microsoft ONNX Runtime 1.28.0 CPU DLL을 고정 SHA-256으로 검증하여 배포 폴더에 포함합니다. OCR 런타임의 원격 진단 전송은 끕니다.

```powershell
.\dist\moasearch.exe doctor | Out-Host
.\start.ps1 -DataDir .moasearch-dev
```

## Python 버전에서 전환

기본 저장 위치 `%LOCALAPPDATA%\MoaSearch`와 기존 SQLite DB 구조를 유지합니다. 폴더·파일·본문·설정은 보존하며, Qwen 모델 설정은 해제합니다. 기존 Qwen 모델 파일은 자동 삭제하지 않지만 Rust 앱은 로드하지 않습니다. 지원하는 Google 모델과 다른 파일은 추론 전에 검증에서 거부합니다.

1. 이전 Python 앱이 실행 중이면 종료합니다.
2. Rust 앱을 빌드하고 `setup-ai.ps1`로 Google 모델을 준비합니다. 다른 데이터 폴더를 사용했다면 `-DataDir`에 같은 경로를 지정합니다.
3. 앱을 열고 **다시 색인**을 누릅니다. Google 임베딩은 이전 모델의 벡터와 호환되지 않아 다시 생성해야 합니다. Rust로 변경된 본문 리더도 함께 적용합니다.

전환 중 기존 본문은 키워드 검색으로 사용할 수 있습니다. 모델 ID가 다른 벡터는 의미 검색에서 섞지 않습니다. 기존 `.venv`와 다운로드한 이전 모델은 실행에 필요하지 않습니다.

## 사용 방법

1. **폴더 추가**에서 검색할 폴더 경로를 입력합니다. 등록 전에는 전체 PC를 검색하지 않습니다.
2. **색인 시작**을 누릅니다. 변경된 파일만 읽고, 완료된 파일을 묶어 저장합니다. 진행 상태를 확인하거나 취소할 수 있습니다.
3. 자연어로 파일을 찾습니다. **의미 + 키워드**, **키워드만**, 확장자 필터를 지원합니다.
4. 결과를 선택하면 본문과 페이지·구간 위치를 볼 수 있습니다. **내 파일에 질문**은 Google Gemma 모델로 근거 문서를 인용하여 답합니다.

자동 변경 확인 기본값은 300초이며 0으로 끌 수 있습니다. 현재 배포 빌드는 CPU에서 실행합니다. 검색 모델과 답변 모델을 번갈아 로드하므로 첫 질의와 첫 답변에는 모델 준비 시간이 추가됩니다.

## 읽을 수 있는 파일

- 텍스트·코드·설정: TXT, Markdown, CSV, JSON, XML, HTML, YAML, TOML, 소스 코드, 자막 등
- 문서: PDF, DOCX, XLSX, PPTX, ODT/ODS/ODP, HWP5, HWPX
- 이미지 OCR: PNG, JPEG, BMP, TIFF, WebP
- 스캔 PDF: 텍스트를 먼저 읽고, 글자가 부족하고 페이지 콘텐츠가 있으면 Windows PDF 렌더링과 Rust OCR 적용

문서당 최대 200,000자, PDF 최대 250페이지를 읽습니다. 파일 크기는 기본 32 MiB, 설정 최대 512 MiB이고 PDF·이미지는 별도로 100 MiB를 넘지 않습니다. ZIP 내부는 항목당 16 MiB·전체 128 MiB, 이미지 프레임당 최대 40 MP·최대 16프레임입니다. PDF 렌더링은 최대 200 DPI·긴 변 3200픽셀·8 MP로 제한합니다. 일부 PDF 페이지를 읽지 못하면 정상 추출한 본문을 보존하고 오류를 표시하여 다음 색인에서 재시도합니다.

HWP3, 암호화·배포용 HWP, 구형 DOC, 오디오 전사·동영상 분석은 지원하지 않습니다. 지원하지 않는 형식은 파일명·경로로 검색합니다. OCR 품질은 인쇄 품질·배치에 따라 달라지며 손글씨 정확도를 보장하지 않습니다. 이번 버전은 EmbeddingGemma 2의 텍스트 임베딩을 사용하며, 이미지·음성을 직접 임베딩하는 DigUp의 모든 기능을 구현한 것은 아닙니다.

## 개인정보와 저장

원본 파일을 이동하거나 수정하지 않습니다. 등록한 폴더 밖, 자격증명 파일, 시스템·캐시·모델·빌드 폴더, 심볼릭 링크와 junction, 온라인 전용 클라우드 자리표시자는 기본 제외합니다. 실행 파일과 스크립트는 결과 화면의 파일 열기로 실행하지 않습니다.

모델 다운로드 외에는 파일 내용·검색어를 외부로 보내지 않습니다. 앱의 WebView 화면과 Rust 서비스는 내장 명령 브리지로 통신하고 TCP 포트를 열지 않습니다. Windows 명명된 파이프는 사용자와 데이터 위치에 해당하는 앱 인스턴스에 탐색기 요청을 전달하는 데만 사용합니다. 원본 문서는 신뢰하지 않는 검색 자료이며 답변 모델에 파일 실행·수정·외부 도구 호출 권한을 주지 않습니다. 파일 검증과 검색 범위를 확인합니다.

## 명령행

```powershell
.\dist\moasearch.exe models list | Out-Host
.\dist\moasearch.exe models download embedding | Out-Host
.\dist\moasearch.exe models download chat | Out-Host
.\dist\moasearch.exe models download ocr | Out-Host
.\dist\moasearch.exe index "$env:USERPROFILE\Documents" | Out-Host
.\dist\moasearch.exe search "지난 분기 회의 일정" | Out-Host
.\dist\moasearch.exe search --keyword "invoice-2026-031" | Out-Host
.\dist\moasearch.exe answer "보증금을 언제 돌려받나요?" | Out-Host
.\dist\moasearch.exe --data-dir "D:\MoaSearchData" instance | Out-Host
.\dist\moasearch.exe --data-dir "D:\MoaSearchData" quit | Out-Host
```

Windows GUI 실행 파일이므로 PowerShell에서 명령 완료와 출력을 기다릴 때는 예제처럼 `| Out-Host`를 붙입니다. `--data-dir` 또는 `MOASEARCH_DATA_DIR`로 모든 명령의 저장 위치를 바꿀 수 있습니다. 모델이 없어도 키워드 색인과 검색은 사용할 수 있습니다.

## 개발·검증

```powershell
.\build.ps1 -Test
cargo fmt --check
node --test tests/frontend_bridge.test.cjs
.\scripts\test-explorer-launch.ps1
.\build.ps1 -PrepareOnly
cargo run --release --example benchmark -- --files 5000
```

화면 브리지 테스트에는 Node.js 18 이상이 필요합니다. 자동 테스트는 임시 가상 문서와 가짜 임베딩을 사용합니다. 별도 실제 모델·OCR 검증은 생성한 예제만 읽도록 분리합니다. [구조와 전환](docs/architecture.md), [성능 기록](docs/performance.md)을 참고하세요. 성능 기록의 Python 측정값과 Rust 측정값은 구분합니다.

실제 모델 검증은 준비된 모델 폴더를 지정합니다. 아래 경로를 자신의 모델 설치 위치로 바꿀 수 있습니다.

```powershell
.\build.ps1 -PrepareOnly
$env:MOASEARCH_OCR_MODELS = "$PWD\.moasearch-rust-dev\models"
cargo test --release --locked --test rust_formats -- --ignored
cargo run --release --locked --example smoke_ai -- .moasearch-verify-ai .moasearch-rust-dev/models
cargo run --release --locked --example smoke_formats -- .moasearch-verify-formats .moasearch-rust-dev/models
```

## 모델·구현 출처

- [Google EmbeddingGemma 2](https://huggingface.co/google/embeddinggemma-2), [llama.cpp용 Q8_0 변환](https://huggingface.co/ggml-org/embeddinggemma-2-GGUF): 텍스트 의미 검색, 약 310 MB.
- [Google Gemma 4 E2B QAT Q4_0 GGUF](https://huggingface.co/google/gemma-4-E2B-it-qat-q4_0-gguf): 문서 기반 답변, 약 3.35 GB.
- 고정 revision·SHA-256과 다운로드 주소는 `src/models.rs`에 있습니다. 두 모델의 라이선스는 Apache-2.0입니다.
- [OAR-OCR](https://github.com/GreatV/oar-ocr), [RapidOCR 모델](https://rapidai.github.io/RapidOCRDocs/main/en/model_licenses/): Rust ONNX OCR 파이프라인과 한국어 전용 OCR 모델. 사전 출처는 `assets/ocr/README.md`에 기록합니다.
- [llama.cpp](https://github.com/ggml-org/llama.cpp), [llama-cpp-2](https://github.com/utilityai/llama-cpp-rs): 앱 내 추론 엔진. Cargo.lock으로 의존성을 고정합니다.
- [DigUp](https://github.com/ARahim3/DigUp): 로컬 파일 추출·의미 검색과 Google 임베딩 모델 선택의 참고 프로젝트입니다. 모아검색의 코드는 별도로 구현했습니다.

화면을 표시하는 Microsoft WebView2 Runtime이 없는 PC에서는 설치 중 공식 Runtime을 내려받으므로 인터넷이 필요합니다. 포터블 ZIP에서는 포함된 MicrosoftEdgeWebview2Setup.exe를 먼저 실행하세요.
