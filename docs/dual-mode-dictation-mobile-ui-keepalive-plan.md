# 双模式听写 + 移动端界面精简 + 保活稳定性方案

> 状态：**计划阶段，等待 coding 许可**
> 更新日期：2026-08-13
> 涉及范围：`openless-all/remote-client`、`openless-all/scripts/linux-fcitx5-plugin`、`openless-all/app/src-tauri/src/android/lan_server.rs`、`openless-all/app/src-tauri/src/coordinator`、`openless-all/app/android`、`openless-all/app/android/frontend`

## 1. 目标

1. 电脑端支持双模式听写：
   - **RAlt 单击（按下后释放）**：只做转录，返回**非 LLM 清洗**的原文。
   - **RAlt + RCtrl**：转录 + LLM 清洗（走现有 active style pack 的润色管线）。
2. 列出移动端当前界面逻辑，供精简决策。
3. 修复移动端保活不稳定的问题：
   - 长时间不使用后 LAN 服务器被杀后台；
   - 保活自动检测无效/状态不可信。

## 2. 双模式听写方案

### 2.1 现状

- remote-client 现在只监听右 Alt 单击（fcitx5 插件 `DictationKeyEvent`），`start` 固定发 `translation:false`。
- 手机端 LAN 服务收到 `start` 后调用 `coordinator.start_dictation()`，收尾时按 active style pack 决定是否 LLM 润色。
- 没有「强制 Raw（不润色）」的远程入口。
- fcitx5 插件已有 `DictationKeyCombined` 信号，但只对「Alt + 非修饰键」生效；RCtrl 是修饰键，目前 Alt+RCtrl 不会被识别。

### 2.2 快捷键行为

| 操作 | 行为 |
| --- | --- |
| RAlt 按下并释放（无其它键） | 开始「Raw 转录」，返回 ASR 原文 |
| RAlt + RCtrl 按下并释放 | 开始「转录 + 清洗」，返回 LLM 润色结果 |
| RAlt + 其它按键（包括方向键） | 忽略，不触发、不打断听写（保持现状） |

### 2.3 改动点

#### 2.3.1 fcitx5 插件（openless.cpp，仅支持 Wayland）

- 该组合键识别只支持 Wayland / fcitx5 通道，不处理 X11 环境（本项目 remote-client 本就只走 fcitx5/Wayland）。
- 在 `dictationTriggerHeld_` 分支中，额外识别 RCtrl（fcitx5/Wayland 的 RCtrl 键事件）：
  - Alt 按住时按下 RCtrl → 置位 `dictationTriggerCombined_`，发出 `DictationKeyCombined(sym=RCtrl, ...)` 并 `filterAndAccept()`。
  - 其它组合键逻辑保持不变。
- 重新编译 `libopenless.so`（arm64/x86_64）并更新本机插件与 Release 资源。

#### 2.3.2 remote-client（main.rs）

- DBus 信号处理保留 `DictationKeyEvent` / `DictationKeyCombined`，并在收到 `Combined` 时记录组合键 sym。
- RAlt 释放时：
  - 无组合 → 发送 `{"type":"start","mode":"raw",...}`；
  - 组合键为 RCtrl → 发送 `{"type":"start","mode":"clean",...}`；
  - 组合键为其它 → 忽略。
- 停止逻辑不变（toggle 模式第二次干净单击停止；非 toggle 模式再次按下停止）。
- README 更新快捷键说明。

#### 2.3.3 LAN 协议（lan_server.rs + 客户端）

- `start` 消息新增可选字段 `mode`：
  - `"raw"`：转录原文，不调用 LLM 润色；
  - `"clean"`：转录 + 润色（现有 active style pack）；
  - 缺省值：`"clean"`，兼容旧客户端。
- `sessionId` / 心跳 / ACK 等协议不变。

#### 2.3.4 Coordinator（app/src-tauri）

- 新增会话级原子标志 `remote_dictation_raw`（或等价状态）：
  - `begin_session` 时清 false；
  - 新增 `start_dictation_raw()`：置 true 后走 `begin_session`；
  - `start_dictation()` 保持现状（clean）。
- 收尾 dispatch（dictation.rs）读取该标志：
  - raw 为 true → 使用 `builtin_style_pack_for_mode(PolishMode::Raw)`，跳过 LLM 润色；
  - raw 为 false → 现有 active style pack 流程。
- 历史记录仍完整保留 `rawTranscript` / `finalText`，UI 无需额外改动即可区分两种结果。

#### 2.3.5 验收

- RAlt 单击返回的文本与 ASR 原文一致（无 LLM 修改）。
- RAlt+RCtrl 返回润色后文本。
- 两种模式都携带 `sessionId`、1 秒心跳、结果 `resultId` + ACK。
- Alt + 其它按键（包括方向键）仍不触发/不打断。

## 3. 移动端界面现状与精简建议

### 3.1 当前界面逻辑

#### 主壳（FloatingShell）

- 移动端顶部栏：标题 + 设置按钮。
- 底部导航（按已确认的精简决定）：`概览`、`历史`、`风格`（弹出 StyleSheet：润色模式/风格市场）、`设置`。
- 移除 `更多` MoreSheet 中的词汇 / 翻译 / 划词追问入口。
- 桌面侧栏在移动端隐藏。

#### 页面

| 页面 | 当前内容 | 移动端相关度 | 精简决定 |
| --- | --- | --- | --- |
| 概览 Overview | ASR/LLM 提供商状态、今日指标、周期指标、最近识别 | 中：远程听写用户主要关心状态与最近记录 | **保留** |
| 历史 History | 搜索、模式筛选、详情、复制、删除、清空、重新润色、重新转录 | 高 | **保留** |
| 词汇 Vocab | 词典管理 | 低（远程听写场景） | **不保留** |
| 润色模式 Style | 风格包/模式配置 | 低 | **保留** |
| 风格市场 Marketplace | 安装/管理风格包 | 低 | **保留** |
| 翻译 Translation | 翻译目标语言等 | 低 | **不保留** |
| 划词追问 SelectionAsk | 选中文本 QA | 低 | **不保留** |

#### 设置（SettingsModal）

- 移动端为全屏页 + 顶部横向 tab：通用 / 服务 / 隐私 / 高级 / 关于。
- 通用：录音与输入、选区润色、主题、语言（桌面快捷键隐藏）。
- 服务：AI 提供商、网络、本地模型（仅桌面）、风格市场。
- 隐私：权限（麦克风/悬浮窗/无障碍/Shizuku/网络）、数据存储。
- 高级：多模态管线、调试工具（Android 可见）。
- 关于：版本、更新（Android 无自动更新控件）。
- Android 权限面板（AndroidPermissionsPanel）内包含：悬浮窗权限、无障碍、Shizuku、插入策略、悬浮窗触发模式/激活方式/左滑动作/取消滑动/大小、单像素保活、通知保活、保活状态/重启/自测。

#### 首次引导（Onboarding）

- Android 共 6 步：麦克风 → 无障碍 → 悬浮窗权限 → 悬浮窗配置 → ASR 提供商 → LLM 提供商。

#### 悬浮窗（Overlay）

- 常驻小圆钮：点击/长按录音、拖动、左右滑动作、上下滑取消；视觉状态 Idle/Armed/Recording/Processing/Error。

### 3.2 已确认的精简决定

1. **页面**：保留 `概览`、`历史`、`润色模式 Style`、`风格市场 Marketplace`；不保留 `词汇`、`翻译`、`划词追问`。
2. **悬浮窗**：保留原有操作逻辑（点击/长按录音、拖动、左右滑动作、上下滑取消、视觉状态），不做改动。
3. **权限引导（Onboarding）**：保留添加权限引导界面，不删除。
4. **设置界面**：需要按 Android 场景调整，具体调整方向见下。

### 3.3 移动端设置调整方向（待最终确认）

- 保留：权限与保活（麦克风 / 悬浮窗 / 无障碍 / Shizuku / 通知 / 电池优化 + 保活状态）、服务（ASR / LLM 提供商）、风格（润色模式 / 风格市场）、关于（版本 / 日志导出）。
- 隐藏/移除：桌面快捷键、选区润色、主题、语言、本地模型、自动更新等桌面相关项；高级中的调试工具按需保留。
- 保活状态整合为设置内的单卡片（通知保活、单像素保活、电池优化限制、最近错误、一键修复）；是否搬到首页另行确认。

> 后续编码按以上决定实施；尚未确认的项（如首页是否改为远程听写状态页）保持现状，等用户拍板。

## 4. 移动端保活不稳定分析与改造计划

### 4.1 现状与根因

当前保活链路：

```text
Rust watchdog（10s 检查 LAN server）      Kotlin service watchdog（5s 调 nativeEnsureRemoteBackend）
         │                                          │
         └────── 都在同一个 App 进程内 ──────────────┘
                    ↑
            进程被系统杀死后，两者一起消失
```

已知问题：

1. **前台服务类型与场景不匹配**：空闲待命时也以 `FOREGROUND_SERVICE_TYPE_MICROPHONE` 保活；Android 14+ 对后台启动/维持麦克风型前台服务有限制，容易 `SecurityException` 或被杀。
2. **看门狗随进程死亡而消失**：Rust 与 Kotlin 的看门狗都在 App 进程内，进程被杀后没有任何外部调度器负责拉起。
3. **START_STICKY 重启不可靠**：国产 ROM / 电池优化下，粘性服务重启可能被延迟数小时或完全不重启。
4. **单像素悬浮窗不能保进程**：它只是视觉效果，系统不因 1×1 悬浮窗保留进程。
5. **自测只模拟 LAN 服务丢失**：不模拟进程被杀，无法验证“服务被系统重建后自动恢复”的真实路径。
6. **状态 UI 每 3s 轮询**，但后端 `lastCheckAt` 是 Rust 进程内的 watchdog 时间；进程死亡后 UI 可能显示旧时间/误报正常。

### 4.2 改造方案

#### 4.2.1 前台服务类型

- 空闲保活改用 `FOREGROUND_SERVICE_TYPE_SPECIAL_USE`（或系统允许的常驻类型），Manifest 增加 `FOREGROUND_SERVICE_SPECIAL_USE` 权限与 `android:foregroundServiceType="specialUse"` 属性，并声明用途。
- 仅在真正录音/远程听写会话期间提升为 `FOREGROUND_SERVICE_TYPE_MICROPHONE`。
- 这样空闲期不触碰麦克风前台服务限制，录音期仍满足麦克风权限要求。

#### 4.2.2 外部调度保活

- 新增 `AlarmManager` 周期任务（建议 10 分钟）或 `WorkManager` PeriodicWork：
  - 检查服务进程是否存活；
  - 未存活 → 启动 `OpenLessOverlayService`（ACTION_KEEPALIVE_SHOW）；
  - 存活但 LAN server 未监听 → 调 `nativeEnsureRemoteBackend`。
- 新增 `BOOT_COMPLETED` / `MY_PACKAGE_REPLACED` 广播接收器，重启后自动拉起保活服务。
- 开机自启与电池优化白名单作为可选引导（不强制）。

#### 4.2.3 看门狗与诊断

- 把 Kotlin watchdog 检查周期从 5s 改为 10s，并增加：
  - `lastServiceStartAt`、`lastProcessDeathAt`（可用 `onTaskRemoved` / `onDestroy` 记录）、`restartCount`；
  - 每次 `startForeground` 成功/失败都写状态；
  - 新增 JNI 方法读取这些诊断字段，UI 展示真实时间而非进程内时间。
- 保活自测升级为**真实进程级测试**（可选按钮）：
  - 先记录状态 → 通过 `ActivityManager.killBackgroundProcesses` 或自杀式重启模拟进程死亡 → 等待 AlarmManager/粘性服务拉起 → 验证 LAN 服务恢复。
- 通知内容在异常时给出可操作原因（权限/电池优化/服务类型失败），并带“打开设置修复”按钮。

#### 4.2.4 验收标准

- 手机闲置 12 小时（灭屏、无操作）后，电脑端仍能发现并连接 LAN 服务。
- 手动结束 App 进程后，10 秒内（最长 1 分钟）服务自动恢复并重新监听 `45678`。
- 保活状态页显示真实的 `lastServiceStartAt` / `lastProcessDeathAt` / `restartCount`，不再出现“进程已死但 UI 显示正常”。
- 打开/关闭通知保活、单像素保活互不影响；关闭后不再强制拉活。

## 5. 实施顺序

1. fcitx5 插件：识别 RAlt+RCtrl 并发出组合事件。
2. remote-client：双模式热键 + `mode` 协议字段。
3. LAN 服务 + Coordinator：`start_dictation_raw()` / raw 标志 / Raw 风格包分派。
4. 移动端界面精简与设置调整（按已确认范围：保留风格/风格市场，移除词汇/翻译/划词追问，设置裁剪，悬浮窗与权限引导不变）。
5. 移动端保活改造（前台服务类型 → 外部调度 → 诊断自测）。
6. 构建、部署、真机验证。

## 6. 风险与注意

- Android 14+ 对 `specialUse` 前台服务的审核要求：必须在 Manifest 的 `property` 里写清楚用途，否则可能被 Play 审核拒绝（侧载无影响）。
- `WorkManager`/`AlarmManager` 在部分国产 ROM 上仍可能被杀；最终效果取决于是否加入电池优化白名单，UI 需明确引导。
- Raw 模式绕过 LLM 后，历史记录中 `mode=raw` 与现有历史筛选天然兼容。
- 双模式协议向后兼容：旧客户端不传 `mode` 仍走 clean。

## 7. 已确认与待确认

已确认：

- [x] RAlt = Raw 转录，RAlt + RCtrl = 转录 + 清洗。
- [x] RCtrl 组合识别仅支持 Wayland / fcitx5 通道。
- [x] RAlt + 其它按键（包括方向键）不触发、不打断。
- [x] 移动端保留：概览、历史、润色模式、风格市场、设置、悬浮窗原逻辑、权限引导界面。
- [x] 移动端不保留：词汇、翻译、划词追问。

待确认：

- [ ] 移动端设置裁剪的具体范围（3.3 的方向是否照此实施）。
- [ ] 保活方案是否采用 `specialUse` + AlarmManager/WorkManager。
- [ ] 首页是否改为远程听写状态页（默认保持现状）。
- [ ] 获得 coding 许可后开始实施。
