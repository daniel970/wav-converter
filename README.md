# WAV 일괄 변환기 (wav-converter)

폴더 안의 모든 음원 파일을 **WAV로 일괄 변환**하는 Windows용 GUI 도구.
입력 폴더의 하위 폴더 구조를 그대로 복제해서 출력 폴더에 저장합니다.

- **순수 Rust** — 외부 프로그램 설치 불필요. `wav-converter.exe` 하나만 복사하면 어디서든 실행.
- **거의 모든 포맷 디코딩** — mp3, flac, m4a/aac, alac, ogg, wav, aiff 등 (symphonia)
- **출력 규격 선택** — 원본 유지(무손실) / 16bit·44.1k / 16bit·48k / 24bit·48k / 24bit·96k
- **드래그&드롭** — 폴더를 창에 끌어다 놓으면 입력 폴더로 지정

## 사용법

1. `wav-converter.exe` 실행
2. **입력 폴더 선택** — 변환할 음원들이 들어있는 폴더 (하위 폴더까지 모두 탐색)
3. **출력 폴더 선택** — WAV가 저장될 폴더 (입력 폴더 구조를 그대로 복제)
4. **출력 규격** 드롭다운에서 원하는 형식 선택
5. **변환 시작** — 진행률과 파일별 결과가 표시됨

예) 입력 `C:\음악`, 출력 `D:\wav`, 파일 `C:\음악\앨범A\track1.mp3`
→ 결과 `D:\wav\앨범A\track1.wav`

## 빌드 (Windows)

[Rust 설치](https://rustup.rs) 후:

```cmd
cargo build --release
```

결과물: `target\release\wav-converter.exe` (단일 실행 파일)

## 빌드 (macOS/Linux에서 Windows용 크로스 컴파일)

```sh
rustup target add x86_64-pc-windows-gnu
# (mingw-w64 툴체인 필요: macOS는 `brew install mingw-w64`)
cargo build --release --target x86_64-pc-windows-gnu
```

결과물: `target/x86_64-pc-windows-gnu/release/wav-converter.exe`

> macOS/Linux에서 그냥 `cargo run` 하면 같은 GUI가 그대로 떠서 동작 확인도 가능합니다.

## 지원 입력 포맷

mp3, flac, wav, m4a, aac, ogg/oga, aiff/aif/aifc, alac, caf, mp4(오디오), mka, mp1, mp2, wv, ape
등 symphonia가 지원하는 포맷. (지원하지 않거나 손상된 파일은 건너뛰고 로그에 기록)

> Opus, WMA 등 일부 포맷은 디코딩되지 않을 수 있습니다. 그런 파일이 많다면
> ffmpeg 기반 버전이 필요하니 알려주세요.

## 구조

- `src/main.rs` — eframe/egui GUI, 백그라운드 변환 스레드, 진행률 표시
- `src/convert.rs` — 디코딩(symphonia) → 리샘플링(rubato) → WAV 인코딩(hound)
