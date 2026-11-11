# 모아검색 이미지

- `icon.svg`: 기존 앱의 돋보기·더하기 심볼을 확장한 벡터 원본. 기존 녹색 계열과 투명한 바깥 모서리를 유지한다.
- `icon.png`: 모아런처 목록·상세 화면용 512×512 PNG. 앱의 웹 아이콘으로도 포함한다.
- `assets/branding/moasearch.ico`: Windows 실행 파일·설치기용 아이콘. 16, 24, 32, 48, 64, 128, 256px 프레임을 포함한다.
- `cover.png`: 모아런처 상세 화면·저장소 소개용 1672×941 PNG.

아이콘은 기존 벡터 로고에서 Windows WPF로 렌더한다. 재생성 명령은 다음과 같다.

```powershell
powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File scripts/render-icon.ps1
```

커버는 2026-11-12 내장 `image_gen` 도구로 생성했다. 별도 CLI/API 경로는 사용하지 않았다. 생성 결과를 크기 변경·합성 없이 그대로 저장했다.

최종 프롬프트:

```text
Use case: ads-marketing. Asset type: final cover for the Windows app Moa Search. Create a polished landscape 16:9 cover. Composition: on the left, the exact Korean title “모아검색” in beautifully spaced dark forest-green typography; beneath it, the exact smaller subtitle “내 파일을 위한 검색”. On the right, an elegant editorial 3D arrangement of warm ivory document sheets and a soft sage file folder beside one prominent rounded-square forest-green application tile. The tile shows a simple ivory monoline magnifying glass with a small plus sign inside the lens, matching the recognizable existing app symbol. Evoke finding the meaning inside personal documents. Warm ivory #f7f7f4 background with a soft sage halo, forest green #436f5d and deep green #315846, one restrained terracotta #be7958 accent. Gentle studio lighting and soft natural shadows, tactile matte-satin materials, clean refined geometry, generous whitespace, friendly premium productivity software. Avoid every other word or text, extra logos, watermarks, clutter, busy tech motifs, and UI screenshots.
```

공개 이미지 주소는 `moalauncher.json`의 `iconUrl`·`coverUrl`에 기록한다.
