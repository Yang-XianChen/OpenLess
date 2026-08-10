# OpenLess Remote Client（极简无界面电脑端）

电脑端按住热键 → 手机 OpenLess 录音并处理 → 松开热键 → 手机回传最终文本 → 电脑自动粘贴到当前光标。

## 构建

```bash
cd openless-all/remote-client
cargo build --release
```

二进制位于 `target/release/openless-remote-client`。

## 运行

```bash
./openless-remote-client --address 192.168.1.20:45678 \
  --hotkey "Ctrl+Shift+Space"
```

参数：

- `--address`：手机 IP + 端口（手机 OpenLess 固定监听 `45678`）
- `--hotkey`：全局触发热键，默认 `Ctrl+Shift+Space`，支持 `Ctrl/Alt/Shift/Super` + 字母/数字/F1-F12/Space 等

## 说明

- 手机端需要保持 OpenLess 运行且在同一局域网；手机屏幕建议保持亮屏/前台（后台录音限制后续版本处理）。
- 当前为局域网明文 WebSocket，未做认证，仅在可信网络使用。
- 插入方式为写剪贴板 + 模拟粘贴（macOS Cmd+V，其它 Ctrl+V）。
