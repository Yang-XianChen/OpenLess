# OpenLess All-Platform

This is the current cross-platform OpenLess workspace.

## App Directory

The runnable Tauri app lives in `app/`. The macOS build links a vendored C ASR engine (`Open-Less/qwen-asr`, forked from `antirez/qwen-asr`) tracked as a git submodule under `app/src-tauri/vendor/qwen-asr/`, so initialize submodules on first clone.

```bash
# First clone only — pull in vendored submodules
git submodule update --init --recursive

cd app
npm ci
npm run tauri dev
```

## 局域网远程听写（Android ↔ PC）

Android 版 OpenLess 启动后在 `0.0.0.0:45678` 监听 WebSocket（见
`app/src-tauri/src/android/lan_server.rs`）。电脑端可用极简无界面客户端
`remote-client/` 通过全局热键触发手机录音/处理，并把最终文本粘贴到电脑当前光标：

```bash
cd remote-client
cargo build --release
./target/release/openless-remote-client --address 192.168.1.20:45678 \
  --hotkey "Ctrl+Shift+Space"
```

按住所选热键 = 手机开始录音；松开 = 手机结束会话、回传润色后的文本并粘贴。
协议与限制见 `remote-client/README.md`。

## macOS Build

Use the project build script instead of calling `tauri build` directly:

```bash
cd app
INSTALL=0 ./scripts/build-mac.sh
```

Generated macOS artifacts:

- `app/src-tauri/target/release/bundle/macos/OpenLess.app`
- `app/src-tauri/target/release/bundle/dmg/OpenLess_1.1.0_aarch64.dmg`

For local install during development:

```bash
cd app
./scripts/build-mac.sh
```

## Windows Build

The runnable Tauri app is still `app/`. Windows contributors should run a
preflight before building so missing MSVC, Windows SDK, or MinGW tools fail
with actionable messages.

```powershell
cd app
powershell -ExecutionPolicy Bypass -File .\scripts\windows-preflight.ps1
```

### MSVC Route

Use this route when Visual Studio Build Tools and the Windows SDK are installed.
Open a Developer PowerShell, or call `vcvars64.bat`, then run:

```powershell
cd app
npm ci
npm run tauri -- build
```

Required Visual Studio Installer components:

- `Microsoft.VisualStudio.Workload.VCTools`
- MSVC v143 x64/x86 build tools
- Windows 10/11 SDK that provides `kernel32.lib`

If `link.exe` or `kernel32.lib` is missing, rerun:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\windows-preflight.ps1 -Toolchain msvc
```

### GNU / MinGW Route

Use this route when MSVC/Windows SDK is unavailable. The app now lives under
the no-space `openless-all` directory to avoid GNU/MinGW path quoting issues
while generating import libraries. Use the helper script to keep the GNU build
environment and target setup consistent.

```powershell
cd app
scoop install rustup mingw
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup target add x86_64-pc-windows-gnu
powershell -ExecutionPolicy Bypass -File .\scripts\windows-preflight.ps1 -Toolchain gnu
powershell -ExecutionPolicy Bypass -File .\scripts\windows-build-gnu.ps1
```

Generated GNU artifacts:

- `%TEMP%\openless-windows-gnu\src-tauri\target\x86_64-pc-windows-gnu\release\openless.exe`
- `%TEMP%\openless-windows-gnu\src-tauri\target\x86_64-pc-windows-gnu\release\bundle\msi\OpenLess_*_x64_en-US.msi`
- `%TEMP%\openless-windows-gnu\src-tauri\target\x86_64-pc-windows-gnu\release\bundle\nsis\OpenLess_*_x64-setup.exe`

### Hotkey Injection Gate

Use this gate before/after Windows hotkey changes when a physical keyboard
regression is unavailable. It injects a dev/test-only hotkey click through the
coordinator `handle_pressed` / `handle_released` path, asserts the log contains
`[coord] hotkey pressed`, and cancels the dry-run session automatically.

```powershell
cd app
npm run check:hotkey-injection
```

### Windows Runtime Notes

- Windows does not need the macOS Accessibility permission. Use Settings ->
  Permissions -> Global hotkey to inspect listener status.
- Microphone permission is checked by opening a short-lived input stream, so a
  device-format query alone is not treated as permission granted.
- Text insertion through `Ctrl+V` is treated as copy fallback unless the app can
  confirm insertion.

## 发布说明

桌面端源码保留备用，但本仓库已停止桌面端发布。当前发布物只有 Android LAN APK 与 remote-client，发布流程见 `.github/workflows/android-lan-release.yml`。

## Ignored Local Output

The following are intentionally local-only:

- `app/node_modules/`
- `app/dist/`
- `app/src-tauri/target/`
- `app/src-tauri/gen/`
- `.DS_Store`
