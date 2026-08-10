# OpenLess（Yang-XianChen Fork）

> 基于 [Open-Less/openless](https://github.com/Open-Less/openless) 的个人 fork。
> 原版 README：[英文版](README.original.md) · [中文版](README.zh.md)

OpenLess 是一款开源语音输入工具：按住热键说话，AI 负责转写、润色，并把最终文本插入到当前光标处。本 fork 在保留上游桌面版能力（macOS / Windows / Linux）的基础上，主要新增了 **Android 端**、**局域网远程听写** 和 **Linux 安装包 / 电脑端客户端**。

## 这个 fork 新增了什么

- **Android 平台支持**：完整的 Android 应用代码与 CI 构建，包含麦克风录音、ASR/润色、悬浮窗、无障碍插入、Shizuku 辅助、凭据保管库和应用内更新等能力。
- **局域网远程听写（Android ↔ PC）**：Android 端启动后监听 `0.0.0.0:45678`，电脑端通过 WebSocket 触发热键录音，手机完成听写后将文本回传并插入电脑当前光标。
- **电脑端 remote-client**：极简无界面 Linux 客户端，支持局域网自动发现、热键触发、切换/按住模式、连接状态提示，以及 Wayland 下的 fcitx5 输入法通道（`--fcitx`）。
- **Linux arm64 安装包**：本地交叉编译的 OpenLess v1.3.16 `.deb`，并附带 fcitx5 插件。
- **V10 增强**：后台自动发现与心跳、断线提示与自动重连提示、错误透传、保活悬浮层，以及一对多连接的会话锁（同一时刻仅一个客户端持有听写会话）。

## 已发布版本与资产

| Release | 内容 | 说明 |
| --- | --- | --- |
| [android-lan-v10](https://github.com/Yang-XianChen/OpenLess/releases/tag/android-lan-v10)（最新） | Android arm64-v8a APK + Linux aarch64/x86_64 remote-client | 局域网远程听写版；APK 为 release 签名构建 |
| [v1.3.16-tauri](https://github.com/Yang-XianChen/OpenLess/releases/tag/v1.3.16-tauri) | `OpenLess_1.3.16_arm64.deb` | Linux arm64 安装包，含 fcitx5 插件，本地未签名构建 |
| android-lan-v9 | 已从 Releases 删除 | 旧版局域网发布，请改用 V10 |

### android-lan-v10 包含的组件

| 资产 | 平台 | 作用 |
| --- | --- | --- |
| `OpenLess_1.3.16_android-lan_arm64-v8a.apk` | Android（arm64-v8a） | 手机端 OpenLess 1.3.16，负责录音、ASR/润色并回传文本；监听 `45678` 端口 |
| `openless-remote-client-linux-aarch64` | Linux（ARM64） | 电脑端无界面客户端，热键触发手机听写并把结果粘贴到当前光标 |
| `openless-remote-client-linux-x86_64` | Linux（x86_64） | 同上，适用于 x86_64 架构电脑 |

## 快速开始

### 1. 安装手机端

从 [android-lan-v10](https://github.com/Yang-XianChen/OpenLess/releases/tag/android-lan-v10) 下载 `OpenLess_1.3.16_android-lan_arm64-v8a.apk` 并安装。打开 OpenLess 后，应用会在 `0.0.0.0:45678` 监听局域网连接；使用期间请保持手机与电脑在同一可信网络，并尽量保持应用前台/亮屏。

> 若之前安装过旧版，覆盖安装前请先卸载旧版。

### 2. 运行电脑端客户端

根据电脑架构下载对应 remote-client 并赋予执行权限：

```bash
chmod +x ./openless-remote-client-linux-x86_64
./openless-remote-client-linux-x86_64 --auto-discover --hotkey "RightAlt" --fcitx
```

常用参数：

- `--address <phone-ip>:45678`：直接指定手机地址
- `--auto-discover`：自动扫描局域网中监听 `45678` 的 OpenLess 手机
- `--hotkey`：全局触发热键，默认 `Ctrl+Shift+Space`
- `--toggle`：按一下开始、再按一下结束；不传则默认按住说话
- `--fcitx`：通过 fcitx5 OpenLess 插件通道监听热键并显示状态（Wayland 下使用）

协议、安全限制与完整参数见 [openless-all/remote-client/README.md](openless-all/remote-client/README.md)。

## 仓库结构

- `openless-all/app/`：Tauri 2 + Rust + React/TS 主应用，包含 Android 平台代码
- `openless-all/remote-client/`：电脑端无界面局域网客户端
- `openless-all/README.md`：全平台工作区说明
- `docs/`：Android、ASR、构建与发布相关文档

## 构建

```bash
git submodule update --init --recursive
cd openless-all/app
npm ci
npm run tauri dev
```

Android APK 与 remote-client 的构建、签名和发布流程见 `.github/workflows/android-lan-release.yml` 以及各子项目 README。发布约定见 [RELEASING.md](RELEASING.md)。

## 许可

[MIT](LICENSE)
