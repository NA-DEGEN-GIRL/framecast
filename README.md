# Framecast

<img src="assets/framecast.png" width="96" alt="Framecast icon">

웹게임·앱 소개 영상을 "그냥 화면 녹화"보다 훨씬 보기 좋게 만들어 주는 Windows용 화면 녹화기입니다.
(FocuSee / Screen Studio 류의 기능을 Rust로 직접 구현)

녹화할 때는 **원본 화면 + 커서/클릭/단축키 이벤트**를 따로 저장하고, 편집기에서 나중에 스타일을 입혀 MP4로 내보냅니다.
그래서 녹화 후에도 줌, 커서 크기, 배경 등을 자유롭게 바꿀 수 있습니다.

## 기능

| 분류 | 내용 |
|---|---|
| 녹화 | **전체 화면 / 특정 창 / 영역** 선택, 모니터 선택, 30/60fps, 3초 카운트다운, 시스템 소리 / 마이크 녹음, 단축키 `Ctrl+Shift+F9` 로 정지 |
| 일시정지 | 녹화 바의 ❚❚ 버튼 또는 `Ctrl+Shift+F10` — 멈춘 구간은 영상·소리·클릭에서 모두 빠져서 이어 붙은 것처럼 재생되고, 편집기 타임라인에 노란 표시로 남음 |
| 창·영역 선택 | 시작 시 화면이 멈춘 상태로 어둡게 표시 — 창은 마우스를 올리고 클릭, 영역은 드래그 (Shift = 16:9 고정), Esc 취소. 창 녹화는 다른 창에 가려져도 그 창만 깨끗하게 찍히고, 창을 움직여도 커서/클릭 위치가 따라감 |
| 녹화 컨트롤 | 화면 상단의 작은 정지 바 — 녹화 영상에는 **찍히지 않음** (`WDA_EXCLUDEFROMCAPTURE`) |
| 자동 줌 | 클릭 위치를 기준으로 줌 구간 자동 생성, 줌 중엔 커서를 부드럽게 따라감 (스프링 + 데드존) |
| 커서 | OS 커서 대신 벡터 커서를 다시 그림 (어떤 줌에서도 선명), 크기 조절, 움직임 스무딩, 대기 시 자동 숨김, 클릭 시 눌림 효과 |
| 클릭 효과 | Ripple / Pulse |
| 프레임 | 그라데이션 배경 8종 또는 **내 이미지**(PNG/JPG/WebP 등, 화면 꽉 채우기 + 흐림 조절), 여백, 둥근 모서리, 그림자, 비율(원본/16:9/4:3/1:1/9:16), 해상도(720p~4K) |
| 단축키 표시 | `Ctrl + C` 같은 단축키를 화면 하단에 배지로 표시 (기본값: 단축키만 기록, 일반 타이핑은 기록 안 함) |
| 모션 블러 | 줌/패닝이 빠를 때만 은은하게 적용 |
| 편집기 | 실시간 미리보기 + 오디오 재생, 타임라인(트림, 줌 블록 추가/이동/길이조절/삭제), 실행취소(Ctrl+Z), 자동 저장 |
| 내보내기 | 하드웨어 H.264 + AAC **MP4**, 또는 **GIF**(480/720/960px, 10/15/20fps, 무한 반복) — 백그라운드 진행, 취소 가능 |

### 편집기 조작

- **Space** 재생/일시정지, **←/→** 한 프레임 이동
- 줌 트랙 **더블클릭**: 줌 추가 / 블록 드래그: 이동 / 양 끝 드래그: 길이 / **Delete**: 삭제
- 클립 양 끝 손잡이 드래그: 앞뒤 자르기 (트림)
- **Ctrl+Z / Ctrl+Y**: 실행취소 / 다시실행
- 사이드바 "Auto-zoom on clicks": 클릭 기록으로 줌 구간 다시 생성

## 성능 (개발 PC 실측, release 빌드)

- 4K 60fps 녹화: 프레임 드랍 0 (601/601), CPU 전체의 약 1%, RAM ~100MB
- 10초 4K 원본 → 1080p60 내보내기: 약 4초

캡처(Windows.Graphics.Capture) → 인코딩(Media Foundation 하드웨어 H.264) → 합성(D3D11 셰이더 + Direct2D) 전 과정이
GPU 텍스처로만 이동하고 CPU 픽셀 복사가 없어서 렉이 거의 없습니다.

## 빌드 / 실행

요구사항: Windows 10 2004+ (Windows 11 권장), Rust (MSVC 툴체인)

```bash
cargo build --release --locked
```

결과물: `target/release/framecast.exe` (단일 실행 파일, ~0.7MB, 런타임 설치 불필요)

녹화 파일 위치: `%USERPROFILE%\Videos\Framecast\<날짜_시간>\`
(`raw.mp4`, `events.json`, `project.json`, `system.wav`, `mic.wav`) — 내보낸 영상은 `Videos\Framecast\Exports\`

## 의존성 / 보안 정책

외부 라이브러리는 **Microsoft 공식 `windows` 크레이트 하나**만 사용하며, 정확한 버전(`=0.62.2`)으로 고정하고
`Cargo.lock`을 커밋합니다. 그 외(JSON, WAV, UI 위젯, 셰이더 등)는 모두 직접 구현했습니다. 자세한 내용은 [SECURITY.md](SECURITY.md).

## 구조

| 파일 | 역할 |
|---|---|
| `src/capture.rs` | Windows.Graphics.Capture 캡처 (모니터 / 창 / 영역 크롭), 창 목록 |
| `src/selector.rs` | 창·영역 선택 오버레이 |
| `src/media.rs` | Media Foundation 하드웨어 인코더/디코더 (GPU 텍스처 입출력) |
| `src/audio.rs` | WASAPI 시스템 소리(loopback)/마이크 캡처, 믹싱, 미리듣기 |
| `src/input.rs` | 저수준 마우스/키보드 훅, 커서 모양 샘플링, 정지 단축키 |
| `src/recorder.rs` | 녹화 스레드 (고정 fps 샘플링 + 이벤트 로그) |
| `src/project.rs` | 프로젝트 데이터, 자동 줌, 스프링 기반 카메라/커서 움직임 계산 |
| `src/render.rs` | 합성기: HLSL 셰이더(배경/그림자/둥근 프레임/줌/모션블러) + Direct2D(커서/클릭/단축키) |
| `src/export.rs` | 내보내기 스레드 (MP4 / GIF) |
| `src/gif.rs` | 자체 GIF 인코더 (변경 영역만 저장, 프레임별 팔레트, LZW) |
| `src/icon.rs`, `build.rs` | 앱 아이콘 생성(`--make-icon`) 및 exe에 아이콘 리소스 삽입 (빌드 의존성 없음) |
| `src/ui.rs`, `src/window.rs`, `src/app.rs` | Direct2D 기반 자체 UI (홈, 녹화 바, 편집기) |
| `src/image.rs` | 이미지 불러오기(WIC), 파일 선택 창 |
| `src/json.rs` | 최소 JSON 파서/작성기 |

## 디버그용

- `framecast.exe --record <초>`: UI 없이 주 모니터 녹화
- `framecast.exe --export <프로젝트 폴더>`: 해당 프로젝트를 `export_test.mp4`로 내보내기
- `framecast.exe --open <프로젝트 폴더>`: 편집기로 바로 열기
- `framecast.exe --make-icon assets/framecast.ico`: 아이콘 다시 그리기 (`src/icon.rs` 수정 후)
- 환경변수 `FRAMECAST_SHOW_CONTROLS_IN_CAPTURE=1`: 녹화 바가 캡처에 보이게 (스크린샷 테스트용)
- 로그: `Videos\Framecast\framecast.log`

## 아직 없는 것 (다음 후보)

- 웹캠 오버레이
