# Contributing / Участие в разработке

## Prerequisites / Требования

- Windows 10/11 x64 with **HypervisorPlatform** (WHPX) enabled —
  `DISM /Online /Enable-Feature /FeatureName:HypervisorPlatform /All`
  (or the in-app one-click setup).
- Rust stable (pinned by `rust-toolchain.toml`), MSVC target.

## Build, verify, test / Сборка и проверка

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --release -- -D warnings
cargo build --release --locked
cargo test --release --locked
```

CLI self-tests (what CI runs, all must pass EN + RU):

```powershell
target\release\gorillaluxe-cli.exe --self-test          # Phase 1 WHPX vCPU
target\release\gorillaluxe-cli.exe --self-test-gfx --ci-software-gpu
target\release\gorillaluxe-cli.exe --self-test-input
target\release\gorillaluxe-cli.exe --self-test-phase4
```

Live emulator tests are `#[ignore]`d — run manually when a runtime is
installed: `cargo test --release -- --ignored`.

## Rules of the road / Правила

- **No comments churn**: don't add/remove comments in code you didn't
  need to touch; keep new code compact.
- **`unwrap()`/`expect()`** are forbidden on UI-facing paths —
  `wnd_proc` must survive lock poisoning (`into_inner` recovery).
- Logs go through `system::logger`/`log_line` — never `println!` in
  library code, never log tokens or secrets.
- New emulator flags: keep every TCP listener on `127.0.0.1` and
  authenticated (see `SECURITY.md`).
- Frame pipeline: keep the `FrameMail` bounded/latest semantics —
  never grow an unbounded queue between capture and render.
- `runtime.lock.toml` + `KNOWN_GOOD_PACKAGES` must be updated
  together when bumping the runtime version.

## Layout / Структура

See `ARCHITECTURE.md`. CI workflow: `.github/workflows/`.
