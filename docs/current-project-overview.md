# OpenLess 当前项目概况

> 更新日期：2026-08-12  
> 工作分支：`beta`（fork: `Yang-XianChen/OpenLess`）

## 1. 项目定位

OpenLess 是一个语音输入 / 听写工具，核心目标是“说话 → 本地/云端识别 → 润色 → 直接写入当前输入光标”。

当前仓库包含三条产品线：

| 产品线 | 说明 |
| --- | --- |
| OpenLess 桌面端 | Tauri + React + Rust 的完整桌面应用，支持本地/云端 ASR、LLM 润色、历史记录、风格包等 |
| OpenLess Android LAN 版 | 手机端 APK，用于局域网远程听写，启动后监听 `0.0.0.0:45678` WebSocket |
| openless-remote-client | 电脑端无 UI 客户端，通过热键连接手机，触发录音并把结果写回当前光标 |

## 2. 当前仓库状态

- 本地工作区位于 `/home/yangxc/Agent/OpenLess/repo/OpenLess-src`；
- 本地 `beta` 分支与 fork 的 `beta` 分支已同步；
- 最近已推送提交：
  - `e84c528e`：Android 通知保活 + 后端自恢复 + 保活诊断；
  - `e39b1349`：修复 Android 构建中 `jni::` 模块遮蔽问题。

## 3. Android LAN 版现状

### 已实现

- 手机端 OpenLess 启动后自动监听 `0.0.0.0:45678`；
- 支持多个桌面客户端连接，但同一时刻只允许一个活跃听写会话；
- 支持远程 `start` / `stop` / `cancel` / `ping`；
- 支持锁屏/后台录音的前台麦克风服务；
- 支持单像素悬浮窗保活；
- 新增“通知保活”：常驻“远程听写待命”通知，服务被系统重建时自动恢复 Rust 后端与 LAN 服务；
- 新增保活诊断状态与自测入口；
- AndroidManifest 已补充 `POST_NOTIFICATIONS` 等权限。

### 最近 CI

- GitHub Actions `Android APK (debug)` 构建成功；
- 已生成 arm64 / armv7 / x86 / x86_64 四个 ABI 的 release APK；
- arm64 安装包已下载到本地：
  - `/home/yangxc/Agent/OpenLess/artifacts/ci-artifacts/31560541850/OpenLess_1.3.16_arm64-v8a.apk`

## 4. 电脑端 remote-client 现状

### 部署方式

- 二进制：`/home/yangxc/Agent/OpenLess/deploy/bin/openless-remote-client`
- 启动脚本：`/home/yangxc/Agent/OpenLess/deploy/bin/openless-remote`
- systemd 用户服务：`openless-remote.service`
- 当前参数：`--auto-discover --hotkey RightAlt --toggle --fcitx`

### 当前能力

- 后台每 15 秒扫描局域网；
- 自动发现监听 `45678` 的手机；
- 通过 fcitx5 捕获右 Alt，切换模式下按一下开始、再按一下结束；
- 通过 fcitx5 `CommitText` 写回文本，失败时回退剪贴板；
- 已从 GitHub Release `android-lan-preview` 重新安装 `openless-remote-client-linux-x86_64`。

### 运行状态

- systemd 服务正常运行；
- 客户端已自动发现手机：`192.168.1.211:45678`；
- 手机端重新打开 App 后电脑端可自动重连。

## 5. 当前已知问题

### 会话状态脆弱

- 客户端与服务端通信太少，`start` / `stop` 没有会话 ID；
- 客户端仅靠本地 `recording` 布尔值判断状态；
- 手机端断线时只释放占用，未自动取消录音；
- 可能出现：
  - 提示“连接阻塞”但实际已在录音；
  - 录音中无法关闭；
  - 结果粘贴两次；
  - 关闭后自动重新开始。

### 已实现（2026-08-12）

- 引入 `sessionId` 会话锁：全局同一时刻只有一个活跃听写会话，按 `clientId` 持有；
- 客户端会话期间每 1 秒发送心跳，服务端 3 秒未收到心跳自动取消录音、释放锁并关闭连接；
- `start` / `stop` / `cancel` 幂等，重复 stop 返回 `alreadyStopped`，不会二次粘贴；
- `resultId` + ACK 结果去重：客户端按 resultId 去重，插入成功后回 ACK；
- 新增 `hello` 协议版本握手与 `status` 状态查询；
- 客户端新增单实例锁，第二个实例直接退出；
- 设计文档：`docs/remote-client-session-lock-and-heartbeat-plan.md`

## 6. 桌面端 OpenLess（源码保留备用）

- 仓库仍保留 `openless-all/app` 中的桌面端源码，供后续开发/Android 构建参考；
- 已移除桌面端发布工作流（`release-tauri.yml`）、Homebrew Cask、版本发布脚本与发布政策文档；
- 本仓库不再发布桌面端安装包，只发布 Android LAN APK 与 remote-client；
- 桌面端与 remote-client 相互独立，remote-client 不依赖桌面 UI。

## 7. 常用命令

```bash
# 构建 Android APK（本地有 NDK 时）
cd openless-all/app
npm run tauri:android:build:debug

# 通过 GitHub CI 构建
gh workflow run 331152587 --repo Yang-XianChen/OpenLess --ref beta

# 重启 remote-client
systemctl --user restart openless-remote

# 查看 remote-client 日志
journalctl --user -u openless-remote -n 100
```

## 8. 下一步计划

1. 实现 `sessionId` 会话锁与 1 秒心跳；
2. 实现断线/心跳超时自动取消录音；
3. 实现 `status` 查询与客户端状态机；
4. 实现 `start` / `stop` 幂等与结果 ACK 去重；
5. 增加客户端单实例锁；
6. 协议版本握手与日志完善；
7. 重新构建 Android APK 与 remote-client 并验证。
