# 가상 문서 픽스처

모두 이전 개발 단계의 smoke 검증에서 직접 생성한 문서다. 사용자 문서는 포함하지 않는다.

- `hwp-aurora.hwp`: HWP5 CFB 문서, 가상 식별자 `오로라-3147`.
- `hwpx-cyan.hwpx`: HWPX 본문·표, 가상 식별자 `청록-9281`, `나침반-6602`.
- `receipt.png`: 한국어 출장 정산 가상 영수증.
- `meeting.pdf`: 한국어 서비스 운영 가상 회의록을 이미지로 넣은 스캔 PDF.
- `rental.pdf`: 디지털 영문 표지와 한국어 보증금 반환 조건의 스캔 페이지를 합친 가상 PDF.

Python 생성기 없이 Rust 테스트에서 그대로 읽는다. Rust OAR/ONNX OCR을 실제 실행하는 테스트는 모델 경로를 지정하고 별도 명령으로 실행한다.

```powershell
.\build.ps1 -PrepareOnly
$env:PATH = "$PWD\target\release;$env:PATH"
$env:MOASEARCH_OCR_MODELS = "$PWD\.moasearch-rust-dev\models"
cargo test --release --locked --test rust_formats -- --ignored
```
