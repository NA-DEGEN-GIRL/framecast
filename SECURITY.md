# Security & supply chain / 보안 · 공급망

## Dependencies / 의존성

| Crate | Version | Source | Purpose |
|---|---|---|---|
| `windows` | `=0.62.2` | Microsoft (microsoft/windows-rs) | Win32 / WinRT API bindings |
| `windows-numerics` | `=0.3.1` | Microsoft | Direct2D matrix/vector types (already used by `windows`) |

The locked tree (15 crates) contains only Microsoft's `windows-*` crates plus `syn` / `quote` / `proc-macro2` /
`unicode-ident`, the standard build-time crates their macros use. At runtime the only external code is Windows itself
(Direct3D 11, Media Foundation, WASAPI, WIC, …).

Rules:

- Direct dependencies are pinned with `=x.y.z`.
- `Cargo.lock` is committed; build with `cargo build --release --locked` (fails if the lock file would change).
- No new dependencies by default — JSON, WAV, GIF, UI and resource embedding are implemented in-repo.
- When upgrading, review `cargo tree` and the lock-file diff for any new crate.

잠긴 의존성 트리(15개)는 Microsoft의 `windows-*` 크레이트와, 그 매크로가 쓰는 표준 빌드 타임 크레이트
(`syn`/`quote`/`proc-macro2`/`unicode-ident`)뿐입니다. 직접 의존성은 `=`로 정확히 고정하고, `Cargo.lock`을 커밋하며,
`--locked`로 빌드합니다. 새 의존성은 원칙적으로 추가하지 않습니다.

## Privacy / 개인정보

- Keyboard logging records **shortcuts only** by default (Ctrl/Alt/Win combinations, Enter/Esc/Tab/F-keys).
  Plain typing (e.g. passwords) is recorded only if you enable "Record all keys".
- Everything stays local in `Videos\Framecast`. No network access, no telemetry.
- Recordings capture whatever is on screen — close sensitive windows first, or record a single window/area.

- 키보드는 기본적으로 **단축키만** 기록합니다. 일반 타이핑(비밀번호 등)은 "Record all keys"를 켰을 때만 기록됩니다.
- 모든 데이터는 로컬(`Videos\Framecast`)에만 저장되며 네트워크 통신은 없습니다.
- 녹화는 화면에 보이는 것을 그대로 찍으니, 민감한 창은 닫거나 특정 창/영역만 녹화하세요.

## Reporting / 제보

Please open a GitHub issue (or a private security advisory for anything sensitive).
