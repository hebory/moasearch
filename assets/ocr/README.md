# 한국어 PP-OCRv5 문자 사전

`korean_ppocrv5_dict.txt`는 RapidOCR v3.10.0 한국어 PP-OCRv5 모바일 인식 ONNX
모델의 `character` 메타데이터를 UTF-8로 그대로 추출한 파일입니다. 원래 순서를
유지한 11,945개 항목(한 줄에 Unicode scalar 하나)이며 blank와 space 항목은
포함하지 않습니다. SHA-256:
`a88071c68c01707489baa79ebe0405b7beb5cca229f4fc94cc3ef992328802d7`.

검출·인식 모델의 고정 버전, 출처 URL, SHA-256은 RapidOCR v3.10.0 공식 모델
manifest를 따릅니다.

- Detection: `ch_PP-OCRv5_det_mobile.onnx`, SHA-256
  `4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae`,
  [fixed source](https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/det/ch_PP-OCRv5_det_mobile.onnx).
- Recognition: `korean_PP-OCRv5_rec_mobile.onnx`, SHA-256
  `cd6e2ea50f6943ca7271eb8c56a877a5a90720b7047fe9c41a2e541a25773c9b`,
  [fixed source](https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.10.0/onnx/PP-OCRv5/rec/korean_PP-OCRv5_rec_mobile.onnx).
- Manifest: [RapidOCR v3.10.0 `default_models.yaml`](https://github.com/RapidAI/RapidOCR/blob/v3.10.0/python/rapidocr/default_models.yaml).

## CTC 문자 인덱스

인식 모델 출력 클래스는 총 11,947개입니다. 인덱스 0은 CTC blank, 그 뒤에는
사전의 11,945개 항목이 원래 순서대로 오고, 마지막 클래스는 space입니다.
OAR-OCR 0.10.2는 일반 사전을 읽을 때 blank와 space를 자동으로 추가합니다
(`has_explicit_blank = false`, `use_space_char = true`). 이 파일에는 blank나
space를 추가하지 마세요. OAR-OCR은 줄마다 첫 문자를 사용하며, 각 항목이 단일
Unicode scalar이므로 이 사전에서는 손실이 없습니다.

## 출처와 라이선스

RapidOCR는 PP-OCRv5를 포함한 등록 모델 파일의 라이선스를 Apache-2.0으로
표시하고, 원본 모델의 저작권자를 Baidu 및/또는 PaddleOCR 권리자로 명시합니다.
PaddleOCR 프로젝트도 Apache-2.0입니다. 이 사전은 인식 모델 메타데이터에서
추출했습니다. OCR 리소스를 재배포할 때 위 출처와 라이선스 고지를 유지하세요.
[RapidOCR 모델 라이선스·출처 고지](https://rapidai.github.io/RapidOCRDocs/main/en/model_licenses/),
[PaddleOCR 라이선스](https://github.com/PaddlePaddle/PaddleOCR/blob/main/LICENSE)
