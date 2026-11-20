# 보안 / 공급망 정책

## 의존성

| 크레이트 | 버전 | 출처 | 용도 |
|---|---|---|---|
| `windows` | `=0.62.2` | Microsoft (microsoft/windows-rs) | Win32/WinRT API 바인딩 |
| `windows-numerics` | `=0.3.1` | Microsoft | Direct2D 행렬/벡터 타입 (`windows`가 이미 의존) |

`Cargo.lock`에 잠긴 전체 트리(15개)는 Microsoft의 `windows-*` 크레이트와, 그 매크로가 쓰는
`syn` / `quote` / `proc-macro2` / `unicode-ident`(Rust 생태계 표준 빌드 타임 크레이트)뿐입니다.
런타임에 링크되는 외부 코드는 Windows 자체 DLL(D3D11, Media Foundation, WASAPI 등)이 전부입니다.

규칙:

- 직접 의존성은 반드시 `=x.y.z`로 정확히 고정.
- `Cargo.lock` 커밋, 빌드는 `cargo build --release --locked` (lock 파일이 바뀌면 빌드 실패).
- 새 의존성은 추가하지 않는 것이 원칙. JSON·WAV·UI 등은 직접 구현.
- 버전을 올릴 때는 `cargo tree`로 새로 들어오는 크레이트를 확인하고 lock 파일 diff를 리뷰.

## 개인정보

- 키보드는 기본적으로 **단축키(Ctrl/Alt/Win 조합, Enter/Esc/Tab/F키 등)만** 기록합니다.
  일반 타이핑(비밀번호 등)은 "Record all keys"를 켰을 때만 기록됩니다.
- 모든 데이터는 로컬(`Videos\Framecast`)에만 저장되며 네트워크 통신은 전혀 없습니다.
- 녹화는 화면 전체를 찍으므로, 민감한 창은 녹화 전에 닫거나 내보낸 영상을 확인하세요.
