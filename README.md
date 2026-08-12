# OpenLess（Yang-XianChen Fork）

基于 [Open-Less/openless](https://github.com/Open-Less/openless) 的个人 fork，只发布 Android 端与 Linux 电脑端 remote-client：按住热键说话，手机完成 ASR/润色，文本回传并插入电脑当前光标。

## 这个 Fork 新增了什么

- **Android 平台支持**：完整 Android 应用与 CI 构建，包含麦克风录音、ASR/润色、悬浮窗、无障碍插入、Shizuku 辅助、凭据保管库、应用内更新，以及单像素悬浮窗保活（锁屏/后台继续录音）。
- **局域网远程听写（Android ↔ PC）**：手机监听 `0.0.0.0:45678`，电脑端通过 WebSocket 触发热键录音，文本回传后插入电脑当前光标。
- **电脑端 remote-client**：极简无界面 Linux 客户端，支持局域网自动发现、热键触发、切换/按住模式，以及 Wayland 下的 fcitx5 输入法通道。
- **协议 v2**：会话锁、1 秒心跳与超时自动停录、`resultId` + ACK 结果去重、协议版本握手。

## 最新发布

[android-lan-preview](https://github.com/Yang-XianChen/OpenLess/releases/tag/android-lan-preview) 包含 Android APK（arm64-v8a、armeabi-v7a、x86、x86_64）与 remote-client（aarch64、x86_64）。

## 快速开始

1. 下载并安装对应架构的 APK；手机与电脑保持同一局域网。若安装过旧版，请先卸载。
2. 下载对应架构的 remote-client 并运行：

```bash
chmod +x ./openless-remote-client-linux-x86_64
./openless-remote-client-linux-x86_64 --auto-discover --hotkey "RightAlt" --fcitx
```

完整参数、协议与安全说明见 [remote-client 自述文件](openless-all/remote-client/README.md)。

## 许可

[MIT](LICENSE)
