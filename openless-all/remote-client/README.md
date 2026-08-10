# OpenLess Remote Client（极简无界面电脑端）

电脑端触发热键 → 手机 OpenLess 录音并处理 → 再次触发结束 → 手机回传最终文本 → 电脑自动粘贴到当前光标。

默认 `--toggle` 为切换模式：按一下开始、再按一下结束。不传 `--toggle` 时为按住说话模式。

## 构建

```bash
cd openless-all/remote-client
cargo build --release
```

二进制位于 `target/release/openless-remote-client`。

## 运行

```bash
./openless-remote-client --address 192.168.1.20:45678 \
  --hotkey "RightAlt" --toggle
```

参数：

- `--address`：手机 IP + 端口（手机 OpenLess 固定监听 `45678`）
- `--hotkey`：全局触发热键，默认 `Ctrl+Shift+Space`；支持 `Ctrl/Alt/Shift/Super` + 主键，以及单独 `RightAlt` / `LeftAlt`
- `--toggle`：切换模式（按一下开始，再按一下结束）
- `--fcitx`：通过 fcitx5 OpenLess 插件的 DBus 信号监听热键（Wayland 下捕获右 Alt 等修饰键需要 fcitx5 正在运行）

## 说明

- 手机端需要保持 OpenLess 运行且在同一局域网；手机屏幕建议保持亮屏/前台（后台录音限制后续版本处理）。
- 当前为局域网明文 WebSocket，未做认证，仅在可信网络使用。
- 插入方式为写剪贴板 + 模拟粘贴（macOS Cmd+V，其它 Ctrl+V）。
