# Osmium: 순수 Rust 컨텍스트 믹싱(Context-Mixing) 텍스트 압축기

Osmium은 Hutter Prize 데이터셋(`enwik8`, `enwik9`)으로 검증된 순수 Rust 컨텍스트 믹싱 압축기입니다. 정수 연산만으로 동작하여 네이티브 CLI와 WebAssembly 브라우저 데모(<https://omakasekim.github.io/osmium/>) 간 바이트 단위 완벽 호환을 보장합니다.

English documentation: [`README.md`](./README.md)

## 핵심 성능 및 기능 (v0.3.0)

- **압축률**: `enwik8` (100 MB) → **18,485,784 바이트 (1.479 bits/byte)** (`xz -9e` 24.8 MB, `bzip2 -9` 29.0 MB, `gzip -9` 36.5 MB 대비 대폭 우수).
- **예측 파이프라인**: 19개 컨텍스트 모델(차수 0~8, 단어/마크업/간접 컨텍스트) + 4-way 집합연관 비트 히스토리 + 런/매치(7B, 24B) 모델 + 4개 게이트 로지스틱 믹서 + 4단계 SSE 보정 + 이진 산술 부호화.
- **프레임드 `OSM\x03` 아카이브**: 블록별 CRC-32C 및 전체 스트림 SHA-256 무결성 검증, 메모리 프리셋(`-l 1..7`, ~7 MiB ~ ~9 GiB), 결정론적 병렬 블록 압축(`-b`, `-j`), 가역 UTF-8/위키 마크업 전처리기(`-x`), 스트리밍 파이프(`-`), 검사·검증·엔트로피 분석(`i`, `v`, `a`, `--json`).
- **WebAssembly (`web/`)**: `compress`, `decompress`, `compress_v3`, `inspect_json`, `verify_json`, `analyze_json`, `version` 내보내기.

## 빌드 및 실행

```bash
cargo build --release
./target/release/osmium c sample.txt sample.osm -l 1 -b 64K -j 4 -x --json
./target/release/osmium v sample.osm -j 4 --json
./target/release/osmium a sample.txt -l 1 -x
cargo test --release
```

## 라이선스

MIT License — 자세한 내용은 [`LICENSE`](./LICENSE)를 참고하세요.
