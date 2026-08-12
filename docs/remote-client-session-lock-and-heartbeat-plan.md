# OpenLess 远程听写：会话锁与心跳协议设计

> 状态：已实现（2026-08-12）
> 范围：`openless-all/remote-client`（电脑端）与 `openless-all/app/src-tauri/src/android/lan_server.rs`（手机端）

## 1. 背景与问题

当前协议过于简单，只依赖：

- 客户端本地一个 `recording` 布尔值；
- 手机端一个按 Socket 地址记录的占用表；
- `start` / `stop` 两条消息。

由此导致：

- 客户端与服务端状态容易不一致；
- 客户端断线后，手机端可能继续录音；
- 重复按键可能触发重复 start/stop；
- `stop` 只能由持有原连接的对象调用，重连后无法停止旧会话；
- 可能出现“提示连接阻塞但实际已在录音”“无法关闭”“粘贴两次”等问题。

## 2. 设计目标

- 每个录音会话有唯一 `sessionId`，所有操作都绑定该 ID；
- 会话建立后进入“锁定状态”，只有持有者能操作；
- 客户端必须保持连接并持续发送心跳；
- 断线或心跳超时，手机端自动取消录音并释放锁；
- start/stop 幂等，重复操作不会产生副作用；
- 结果回传有 ACK，避免重复粘贴；
- 客户端状态可查询、可恢复。

## 3. 协议设计

### 3.1 通用字段

所有客户端请求都携带：

```json
{
  "clientId": "客户端进程 UUID"
}
```

会话建立后，所有会话相关请求再携带：

```json
{
  "sessionId": "服务端生成的会话 UUID"
}
```

### 3.2 消息清单

#### 客户端 → 服务端

| type | 必带字段 | 说明 |
| --- | --- | --- |
| `hello` | `clientId`、`protocolVersion` | 连接建立后握手，便于兼容性判断 |
| `start` | `clientId`、`translation` | 发起听写 |
| `ping` | `sessionId` | 会话心跳，默认每 1 秒一次 |
| `stop` | `clientId`、`sessionId` | 结束听写并取回结果 |
| `cancel` | `clientId`、`sessionId` | 取消听写，不等待结果 |
| `status` | `clientId` | 查询当前手机端会话状态 |
| `ack` | `clientId`、`resultId` | 确认已收到结果，防止重复回传 |

#### 服务端 → 客户端

| type | 说明 |
| --- | --- |
| `hello_ok` | 握手成功 |
| `started` | 录音会话已建立，携带 `sessionId` |
| `pong` | 心跳应答 |
| `result` | 听写结果，携带 `sessionId`、`resultId`、`text` |
| `stopped` | 会话已正常结束 |
| `cancelled` | 会话已取消 |
| `error` | 错误，携带 `code` 与 `message` |
| `status` | 当前状态快照 |

## 4. 会话锁规则

### 4.1 start

- 服务端无活跃会话：创建 `sessionId`，进入 `Recording`，返回 `started`。
- 同一 `clientId` 重复 `start`：返回已有 `sessionId`，不重复录音。
- 其他 `clientId` 发起 `start`：返回：

```json
{
  "type": "error",
  "code": "SESSION_BLOCKED",
  "message": "另一个桌面客户端正在使用当前会话",
  "ownerClientId": "...",
  "sessionId": "..."
}
```

### 4.2 stop

- 只允许 `clientId` + `sessionId` 匹配当前持有者的调用。
- 重复 `stop`：返回 `already_stopped`，不产生第二次粘贴。
- 非持有者 `stop`：返回 `SESSION_BLOCKED` 或 `NO_SESSION`。

### 4.3 cancel

- 持有者可随时取消；
- 取消后立即释放会话锁，并停止手机端录音；
- 取消不等待 ASR 结果。

### 4.4 心跳锁

- 客户端在会话期间每 **1 秒** 发送一次：

```json
{"type":"ping","clientId":"...","sessionId":"..."}
```

- 服务端只接受 `sessionId` 匹配当前会话的心跳；
- 超过 **3 秒** 未收到有效心跳，服务端自动：
  1. `cancel_dictation()`；
  2. 释放会话锁；
  3. 关闭该连接；
  4. 记录 `heartbeat_timeout` 日志。

> 参数可后续调整：心跳 1 秒，超时 3 秒，等价于允许丢失 2 次心跳。

## 5. 服务端状态机

```text
Idle
  │ start
  ▼
Recording（持有 sessionId / ownerClientId / lastHeartbeat）
  │ stop
  ▼
Stopping → 等待结果 → Idle
  │ cancel / heartbeat timeout / disconnect
  ▼
Idle
```

服务端必须保证以下清理路径都执行：

- 正常 `stop` 完成；
- 客户端发送 `cancel`；
- 心跳超时；
- WebSocket 断开；
- 服务端处理 panic 或异常退出。

每条路径都要调用：

```rust
coordinator.cancel_dictation();          // 若仍在录音
coordinator.set_remote_capture_mode(false);
release_owner(...);                      // 释放全局会话锁
```

## 6. 客户端状态机

```text
Idle
  → Connecting
  → WaitingStarted
  → Recording（心跳线程运行中）
  → Stopping
  → Inserting
  → Idle
```

每个状态都有超时和失败处理：

| 状态 | 超时 | 失败处理 |
| --- | --- | --- |
| `Connecting` | 3 秒 | 提示连接失败并重试 |
| `WaitingStarted` | 5 秒 | 发送 `cancel` 后回到 Idle，防止手机已开始录音 |
| `Recording` | 心跳发送失败 | 清除本地会话，提示重新扫描 |
| `Stopping` | 90 秒 | 查询 `status`，决定是否重试或取消 |
| `Inserting` | 3 秒 | 回退剪贴板并提示 |

## 7. 结果去重（粘贴两次问题）

服务端返回结果时生成 `resultId`：

```json
{
  "type": "result",
  "sessionId": "...",
  "resultId": "uuid",
  "text": "..."
}
```

客户端：

1. 按 `resultId` 去重；
2. 插入文本前记录已处理 ID；
3. 插入成功后发送：

```json
{"type":"ack","clientId":"...","resultId":"..."}
```

服务端确认收到 ACK 后才删除待回传结果；未收到 ACK 时只重发一次，不做无限重发。

## 8. 其他加固建议

### 8.1 单实例锁

客户端启动时获取单实例锁：

- 已存在运行实例时，第二个实例直接退出；
- 或者把热键事件转发给已运行实例。

这是“连接被阻塞”最常见的来源之一：系统服务已经跑了一个客户端，手动又开了一个。

### 8.2 状态查询与自恢复

新增 `status` 命令，客户端按键前先查询：

```json
{"type":"status","clientId":"..."}
```

服务端返回：

```json
{
  "type": "status",
  "state": "idle | recording | transcribing",
  "sessionId": null,
  "ownerClientId": null,
  "lastError": null
}
```

客户端据此判断是 `start`、`stop`、`cancel` 还是恢复。

### 8.3 协议版本握手

连接建立后先发 `hello`：

```json
{"type":"hello","clientId":"...","protocolVersion":2}
```

版本不兼容时返回明确错误，避免新客户端连旧手机。

### 8.4 日志与诊断

- 客户端记录每个状态切换、心跳发送、错误码；
- 服务端记录 session 创建、心跳刷新、超时取消；
- 日志统一包含 `clientId` / `sessionId`，便于真机问题定位。

## 9. 参数表

| 参数 | 建议值 |
| --- | --- |
| 心跳间隔 | 1 秒 |
| 心跳超时 | 3 秒 |
| `WaitingStarted` 超时 | 5 秒 |
| `Stopping` 超时 | 90 秒 |
| 结果 ACK 重发次数 | 最多 1 次 |
| 会话锁粒度 | 全局唯一，按 `clientId` 持有 |

## 10. 实施顺序

1. 手机端：`sessionId` + 会话锁 + 心跳超时 + 断线自动取消；
2. 电脑端：保存 `sessionId` + 1 秒心跳线程；
3. 电脑端：`status` 查询与状态机恢复；
4. 两端：`start` / `stop` 幂等；
5. 两端：`resultId` + ACK 去重；
6. 电脑端：单实例锁；
7. 协议版本握手与日志完善。

## 11. 验收标准

- 按下热键后，手机端返回带 `sessionId` 的 `started`；
- 会话期间电脑端每 1 秒发送一次心跳；
- 杀掉电脑端进程后，手机端 3 秒内自动取消录音并释放锁；
- 同一客户端重复 `start` 不会重复录音；
- 重复 `stop` 不会粘贴两次；
- 第二个客户端 `start` 会收到包含 `ownerClientId` / `sessionId` 的阻塞错误；
- 断线重连后可通过 `status` 查询并清理残留会话。
