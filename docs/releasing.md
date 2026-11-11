# Windows 릴리스와 Moa Launcher 등록

## 패키지 만들기

배포에는 x64 Windows 실행 파일, ONNX Runtime 1.28.0 DLL 두 개, 해당 라이선스 고지, Microsoft Authenticode로 확인한 MSVC CRT 세 파일이 필요합니다. 이번 배포는 Windows `System32`의 `14.50.35719.0` 파일을 출처로 고정하고, 파일 서명·세 CRT의 공통 버전·llama.cpp 최소 버전 `14.39.0.0`을 확인합니다. 공식 CRT 재배포본을 따로 제공할 때에는 `-CrtDirectory`로 Microsoft 서명된 파일의 디렉터리를 지정하세요. 임의 파일이나 Microsoft 서명이 검증되지 않은 CRT는 패키지에 넣지 않습니다.

검증 당시 파일 출처는 `C:\Windows\System32`이며 CRT 제품 버전은 `14.50.35719.0`입니다. SHA-256은 `MSVCP140.dll` `def46aa6a8f72f27bafac0c43334419486a4d1dcdb6c479a8ef7034b3e1fa4cb`, `VCRUNTIME140.dll` `184146852727a9db4eea06178716bec3cdbb1015c911f6b0f915b184ad7775b2`, `VCRUNTIME140_1.dll` `e6bfb3662ab4b1969a73441dbe35c96d51441b6bff8cf1fe7430bd5b246ca605`입니다. 실제 패키지에서 선택된 출처·버전·서명·체크섬은 생성된 릴리스 명세에도 기록합니다.

패키징 스크립트 실행에는 PowerShell 7(`pwsh`)과 Inno Setup 6 또는 7의 `ISCC.exe`가 필요합니다. 이는 릴리스 제작 도구의 요구사항이며, 설치 사용자에게 PowerShell 7은 필요하지 않습니다. `-SkipBuild`는 Rust 앱 다시 빌드하는 단계만 건너뛰고, ZIP 포장 및 Inno 설치 프로그램 컴파일은 실행합니다.

```powershell
$licenses = .\packaging\collect-licenses.ps1 | ConvertFrom-Json
.\packaging\release.ps1 `
  -Version 0.2.0 `
  -SkipBuild `
  -IsccPath 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' `
  -CrtDirectory "$env:SystemRoot\System32" `
  -LicenseDirectory $licenses.outputDirectory
```

Inno Setup이 `PATH`에 있으면 `-IsccPath`를 생략할 수 있습니다. 라이선스 수집기는 의존성 원본의 고지를 별도의 새 run 폴더에 보관하므로 반환된 `outputDirectory`를 전달합니다. `-Version`의 기본값은 `Cargo.toml` 버전입니다. 앱 파일 버전이 같지 않으면 중단합니다. 같은 버전의 기존 산출물은 덮어쓰지 않습니다.

`dist\artifacts\`에 다음 공개 파일이 생성됩니다.

- `moasearch-0.2.0-win-x64-setup.exe`: Inno Setup 사용자별 설치 프로그램
- `moasearch-0.2.0-win-x64.zip`: 동일한 앱 파일이 최상위에 있는 포터블 ZIP
- `moasearch-0.2.0-win-x64-manifest.json`: 릴리스 자산과 패키지 내부 파일의 SHA-256, CRT 버전·서명, 최소 시스템 요구사항, 설치 위치, 모델 미포함 및 서명 상태
- `SHA256SUMS.txt`: 설치기·ZIP·배포 명세·아이콘·커버·카탈로그 총 6개 파일의 SHA-256 (체크섬 파일 자신은 제외)
- `icon.png`, `cover.png`: Moa Launcher 앱 아이콘과 커버
- `moalauncher.json`: Moa Launcher 카탈로그 등록 메타데이터

릴리스 산출물은 총 일곱 개이며 모두 `dist\artifacts\`에 생성됩니다. 스크립트는 기존 동일 이름 산출물을 덮어쓰지 않습니다. `icon.png`, `cover.png`, `moalauncher.json`은 릴리스 자산으로 복사하지만 실행 ZIP과 설치 파일에는 넣지 않습니다.

두 패키지에는 검색·OCR 모델이 들어 있지 않습니다. 설치 파일에는 앱, ONNX Runtime, Microsoft CRT와 라이선스 고지만 포함합니다. 설치 후 처음 실행하고 나면 앱 설정에서 Google 임베딩·답변 모델과 OCR 모델을 내려받아야 합니다. 합계 약 3.68 GB의 모델 다운로드가 필요합니다. 모델 다운로드가 끝나면 검색·OCR은 로컬에서 실행됩니다. 모델을 개인 데이터 폴더 안에 보관하고 프로그램 설치 폴더와 분리합니다.

인증서로 서명하지 않은 설치 프로그램과 실행 파일은 Windows SmartScreen 경고가 나올 수 있습니다. 공개 릴리스에서는 코드 서명 상태를 실제 게시물에 별도로 설명하세요.

## 설치 동작

설치기는 Windows 10/11 x64에서 `{localappdata}\Programs\moasearch` 아래에 현재 사용자 권한으로 설치합니다. 로컬 AI 기능은 AVX2를 지원하는 x64 CPU가 필요합니다. 관리자 권한을 요구하지 않습니다. `/SILENT /DIR="<경로>" /NORESTART` 옵션을 지원하며, 자동 설치에서는 앱을 시작하지 않습니다. 대화형 설치에서는 완료 화면의 실행 선택 항목을 제공합니다.

직접 설치할 때 Inno Setup은 Restart Manager로 실행 중인 파일을 확인합니다. 모아런처는 앱이 실행 중이면 업데이트를 시작하지 않으므로 먼저 모아검색 실행 창에서 `Ctrl+C`를 누르거나 창을 닫으세요. 브라우저 탭만 닫으면 검색 서버는 계속 실행됩니다. 자동 설치·업데이트 후 앱을 다시 열지 않습니다. 데이터를 `%LOCALAPPDATA%\MoaSearch`에 저장하고, 설치 제거 시 데이터 폴더를 삭제하지 않습니다. 포터블 패키지도 동일한 데이터 위치를 사용합니다.

## Moa Launcher 카탈로그

Moa Launcher의 카탈로그 항목에는 저장소 `hebory/moasearch`와 안정 버전 태그 `v0.2.0`을 등록하고, x64용 Inno 설치 패키지 하나를 연결하세요. 자산 이름은 `moasearch-0.2.0-win-x64-setup.exe`입니다. 포터블 ZIP `moasearch-0.2.0-win-x64.zip`도 직접 다운로드용 릴리스 자산으로 제공하지만 카탈로그 설치 선택지로 등록하지 않습니다. 패키징 스크립트는 카탈로그의 설치 자산 패턴이 생성될 실제 설치기 이름과 맞는지 확인합니다.

Moa Launcher는 앱의 루트에서 공개 `icon.png`와 `cover.png`를 표시합니다. 이 이미지를 저장소 기본 브랜치에 게시하고 카탈로그의 앱 항목에서 해당 GitHub 이미지 주소를 참조하세요. `assets\branding\moasearch.ico`는 Windows 설치기의 아이콘이고, 모아 런처 상세 화면의 PNG 아이콘과 커버를 대체하지 않습니다.

게시 전 설치·ZIP 내용이 allowlist 파일만 포함하는지, CRT 서명과 체크섬 파일을 확인하고 자산별 SHA-256을 교차 검증하세요. GitHub Release 게시에는 `v<버전>` 태그와 다음 일곱 파일을 사용합니다. `SHA256SUMS.txt`에는 체크섬 파일 자신을 제외한 나머지 여섯 파일의 해시가 있습니다.

```text
moasearch-<버전>-win-x64-setup.exe
moasearch-<버전>-win-x64.zip
SHA256SUMS.txt
moasearch-<버전>-win-x64-manifest.json
icon.png
cover.png
moalauncher.json
```

Microsoft Visual C++ Runtime은 배포 가능 구성요소와 계약에 따라 재배포하세요. [Microsoft의 Visual C++ 파일 재배포 안내](https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files)를 확인하고, 필요한 라이선스 고지를 설치 파일에 포함합니다.

## v0.2.0 배포 검증

2026-11-12 Windows x64 환경에서 다음을 확인했습니다.

- `cargo check --release`, 웹 인증·입력 검증 통합 테스트, 최종 네이티브 빌드 성공.
- 설치 EXE 11,516,124바이트, 포터블 ZIP 15,776,032바이트. 여섯 공개 파일의 SHA-256 및 ZIP 내부 738개 파일의 목록·해시가 배포 명세와 일치.
- 한글과 공백을 포함한 경로에 무인 설치 성공. 설치 후 앱이 자동 실행되지 않고 실행 파일 버전은 `0.2.0`으로 확인.
- 설치한 앱에서 HWP·HWPX·이미지·PDF 예제 5개를 오류 없이 색인하여 6개 본문 구간·임베딩 생성. Google 모델의 자연어 검색 `출장 교통비를 청구할 지출 내역`에서 `receipt.png`가 첫 결과.
- 웹 아이콘 정상 응답, ONNX Runtime 및 MSVC CRT 세 DLL을 모두 설치 폴더에서 로드한 사실 확인.
- 제거 프로그램 정상 종료, 실행 파일·설치 등록 제거, 별도 데이터 폴더의 SQLite DB와 보존 확인 파일 유지.

라이선스 수집은 Windows 대상의 일반·빌드 의존성 403개를 포함합니다. 배포 crate에 빠진 원문은 `packaging/license-overrides`의 출처·SHA-256과 함께 보완합니다. `simd_helpers 0.1.0`은 SPDX `MIT` 선언만 있고 확인한 고정 upstream에 라이선스 본문이 없어, 본문을 만들어 넣지 않고 원본의 누락 사실을 고지에 기록했습니다. 깨끗한 Windows VM에서의 검증이나 코드 서명 인증은 이번 검증에 포함되지 않습니다.
