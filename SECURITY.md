# Security Policy / Политика безопасности

## Local attack surface / Локальная поверхность атаки

GorillaLuxe runs Google's Android Emulator locally. All emulator
ports bind `127.0.0.1` only — nothing listens on external interfaces:

| Port | Service | Auth | Назначение |
|---|---|---|---|
| `5554` | Emulator console (`event send`, `avd snapshot`, `kill`) | `.emulator_console_auth_token` (generated per launch, required) | Ввод и управление |
| `5555` | adb transport (`emulator-5554`) | adb protocol (local transport) | adb-мост |
| `8554` | gRPC `EmulatorController` (`streamScreenshot`, `streamInputEvent`) | `-grpc-use-token` — `authorization: Bearer` header required since v0.2 | Захват кадров и ввод |

Removed surface: the QEMU HMP monitor previously listened on
`127.0.0.1:4444` with **no authentication** — any local process could
issue `quit`, `sendkey`, or `stop`/`cont`. It has been removed
completely; graceful shutdown uses the token-authenticated console
`kill` command instead.

Attach hardening: before adopting an already-running `emulator-*`
device, GorillaLuxe verifies via `netstat -ano` that the PID owning
`:5554` is an emulator whose `pid_*.ini` record names our AVD —
a foreign emulator or squatting process is refused and logged.

## Runtime artifact integrity / Целостность артефактов

`runtime.lock.toml` pins the exact emulator / platform-tools /
system-image URLs (compile-time embedded). Every download is
checksum-verified (SHA-256 when published by Google, else SHA-1)
before extraction; archives are extracted with traversal-safe paths.

## Crash data / Данные крашей

Panics write a minidump to `%LOCALAPPDATA%\GorillaLuxe\crash\` and a
backtrace to `logs\gorillaluxe.log`. Dumps may contain process memory
— do not attach them to public issues without review.

## Reporting a vulnerability / Сообщить об уязвимости

Please report security issues privately via GitHub's
"Report a vulnerability" (Security Advisories) on the repository —
do not open a public issue for unpatched vulnerabilities. Include the
GorillaLuxe version/commit, reproduction steps, and the relevant
lines from `%LOCALAPPDATA%\GorillaLuxe\logs\`.
