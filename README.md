# OpenLess（Yang-XianChen Fork）

基于 [Open-Less/openless](https://github.com/Open-Less/openless) 的个人 fork，只发布 Android 端与 Linux 电脑端 remote-client：按住热键说话，手机完成 ASR/润色，文本回传并插入电脑当前光标。

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
