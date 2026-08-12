//! Android 局域网远程听写服务。
#![cfg(target_os = "android")]
//!
//! 手机上的 OpenLess 启动后在 `0.0.0.0:45678` 监听 WebSocket。
//! 电脑端（见 `openless-all/remote-client`）通过局域网连接本服务：
//!
//! ```text
//! PC ── ws://<phone-ip>:45678 ──► OpenLess(Android)
//!        {hello}                → 协议版本握手
//!        {start}                → 开始录音/ASR/润色（携带 clientId）
//!        {ping, sessionId}      → 会话心跳（1 秒一次）
//!        {stop, sessionId}      → 结束会话，等待 final_text
//!        {result, resultId}     → 回传最终文本
//!        {ack, resultId}        → 确认结果，防止重复粘贴
//! ```
//!
//! 协议（JSON 文本帧，字段 `type`）：
//!
//! 客户端 → 服务端：
//! - `{"type":"hello","clientId":"...","protocolVersion":2}`
//! - `{"type":"start","clientId":"...","translation":false}`
//! - `{"type":"ping","clientId":"...","sessionId":"..."}`
//! - `{"type":"stop","clientId":"...","sessionId":"...","translation":false}`
//! - `{"type":"cancel","clientId":"...","sessionId":"..."}`
//! - `{"type":"status","clientId":"..."}`
//! - `{"type":"ack","clientId":"...","resultId":"..."}`
//!
//! 服务端 → 客户端：
//! - `{"type":"hello_ok","protocolVersion":2}`
//! - `{"type":"started","sessionId":"..."}`
//! - `{"type":"pong","sessionId":"..."}`
//! - `{"type":"result","sessionId":"...","resultId":"...","text":"..."}`
//! - `{"type":"stopped","sessionId":"..."}`
//! - `{"type":"cancelled","sessionId":"..."}`
//! - `{"type":"status","state":"idle|recording|transcribing","sessionId":null,"ownerClientId":null}`
//! - `{"type":"error","code":"...","message":"..."}`
//!
//! 会话锁：全局同时只有一个活跃听写会话，按 `clientId` 持有；
//! 会话期间客户端每 1 秒发送心跳，超过 3 秒未心跳自动取消录音并释放锁。
//! 未握手的老客户端仍可用（以连接地址作为伪 clientId），但不会获得会话锁/心跳保护。
//!
//! 安全说明：这是局域网 PoC，未做认证；请仅在可信网络使用。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::coordinator::Coordinator;

/// 默认监听端口（固定，PoC）。
pub const DEFAULT_PORT: u16 = 45678;

/// 当前协议版本。客户端 `hello` 时声明，服务端 `hello_ok` 时返回。
pub const PROTOCOL_VERSION: u32 = 2;

const LAN_STATE_IDLE: u8 = 0;
const LAN_STATE_STARTING: u8 = 1;
const LAN_STATE_LISTENING: u8 = 2;

static LAN_STATE: AtomicU8 = AtomicU8::new(LAN_STATE_IDLE);
static LAN_LAST_ERROR: StdMutex<Option<String>> = StdMutex::new(None);
static LAN_SHUTDOWN: StdMutex<Option<tokio::sync::oneshot::Sender<()>>> = StdMutex::new(None);

/// 等待最终文本落历史的超时。
const RESULT_TIMEOUT: Duration = Duration::from_secs(60);
/// 会话心跳超时：3 秒未收到有效心跳即取消录音并释放锁。
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(3);
/// 心跳检查周期。
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);
/// stop 后等待结果 ACK 的时间；未收到则重发一次结果。
const ACK_WAIT: Duration = Duration::from_secs(3);
/// 同一结果最多发送次数（首发 + 1 次重发）。
const MAX_RESULT_SENDS: u8 = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientCommand {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    result_id: Option<String>,
    #[serde(default)]
    protocol_version: Option<u32>,
    #[serde(default)]
    translation: bool,
}

/// 会话阶段。`Stopping` / `ResultPending` 不再要求心跳（录音已在收尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    Recording,
    Stopping,
    ResultPending,
}

impl SessionPhase {
    fn state_name(self) -> &'static str {
        match self {
            SessionPhase::Recording => "recording",
            SessionPhase::Stopping | SessionPhase::ResultPending => "transcribing",
        }
    }
}

struct PendingResult {
    result_id: String,
    text: String,
}

/// 全局会话锁记录。key 为服务端生成的 `sessionId`。
struct SessionRecord {
    session_id: String,
    owner_client_id: String,
    owner_addr: SocketAddr,
    previous_first_id: Option<String>,
    translation: bool,
    last_heartbeat: Instant,
    phase: SessionPhase,
    result: Option<PendingResult>,
    /// 心跳超时/看门狗关闭该连接用（watch 避免正常结束 drop sender 误触发）。
    shutdown_tx: watch::Sender<bool>,
}

type SessionRegistry = Arc<StdMutex<HashMap<String, SessionRecord>>>;

/// 当前连接上的活跃会话。
struct ActiveSession {
    session_id: String,
    previous_first_id: Option<String>,
    translation: bool,
}

/// 启动 LAN 服务。绑定失败只记日志，不 panic。
pub fn start(coordinator: Arc<Coordinator>) {
    ensure_started(coordinator);
}

/// 幂等启动：已监听时直接返回；启动中不重复发起；失败后允许看门狗再次调用。
pub fn ensure_started(coordinator: Arc<Coordinator>) {
    if LAN_STATE.load(Ordering::Relaxed) != LAN_STATE_IDLE {
        return;
    }
    if LAN_STATE
        .compare_exchange(
            LAN_STATE_IDLE,
            LAN_STATE_STARTING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", DEFAULT_PORT)).await {
            Ok(listener) => listener,
            Err(error) => {
                let message = format!("bind 0.0.0.0:{DEFAULT_PORT} failed: {error}");
                log::error!("[lan-remote] {message}");
                *LAN_LAST_ERROR.lock().unwrap() = Some(message);
                LAN_STATE.store(LAN_STATE_IDLE, Ordering::SeqCst);
                return;
            }
        };
        *LAN_LAST_ERROR.lock().unwrap() = None;
        LAN_STATE.store(LAN_STATE_LISTENING, Ordering::SeqCst);
        log::info!(
            "[lan-remote] listening on 0.0.0.0:{DEFAULT_PORT} (protocol v{PROTOCOL_VERSION})"
        );
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *LAN_SHUTDOWN.lock().unwrap() = Some(shutdown_tx);

        // 全局会话锁：同一时刻只允许一个桌面客户端持有活跃听写会话。
        let registry: SessionRegistry = Arc::new(StdMutex::new(HashMap::new()));
        // start 之间需要串行化，避免两个客户端同时通过“无会话”检查。
        let start_gate = Arc::new(AsyncMutex::new(()));
        spawn_heartbeat_sweeper(Arc::clone(&coordinator), Arc::clone(&registry));

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    log::info!("[lan-remote] listener stopped by keepalive restart");
                    break;
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, addr)) => {
                            let coordinator = Arc::clone(&coordinator);
                            let registry = Arc::clone(&registry);
                            let start_gate = Arc::clone(&start_gate);
                            tauri::async_runtime::spawn(async move {
                                if let Err(error) =
                                    handle_connection(stream, addr, coordinator, registry, start_gate).await
                                {
                                    log::warn!("[lan-remote] connection {addr} ended: {error}");
                                }
                            });
                        }
                        Err(error) => {
                            log::warn!("[lan-remote] accept failed: {error}");
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                    }
                }
            }
        }
    });
}

/// 心跳看门狗：录音阶段超过 `HEARTBEAT_TIMEOUT` 未收到有效心跳时，
/// 自动取消录音、释放会话锁并关闭该连接。
fn spawn_heartbeat_sweeper(coordinator: Arc<Coordinator>, registry: SessionRegistry) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            let stale = {
                let mut reg = registry.lock().unwrap();
                let mut stale = Vec::new();
                reg.retain(|session_id, record| {
                    if record.phase == SessionPhase::Recording
                        && record.last_heartbeat.elapsed() > HEARTBEAT_TIMEOUT
                    {
                        log::warn!(
                            "[lan-remote] session {session_id} heartbeat timeout (owner {}), cancelling",
                            record.owner_client_id
                        );
                        let _ = record.shutdown_tx.send(true);
                        stale.push(record.owner_client_id.clone());
                        false
                    } else {
                        true
                    }
                });
                stale
            };
            for owner in stale {
                log::info!(
                    "[lan-remote] releasing session after heartbeat timeout (owner {owner})"
                );
                coordinator.cancel_dictation();
                coordinator.set_remote_capture_mode(false);
            }
        }
    });
}

/// 当前 LAN 服务是否已成功监听。
pub fn lan_server_is_running() -> bool {
    LAN_STATE.load(Ordering::Relaxed) == LAN_STATE_LISTENING
}

/// 最近一次启动失败原因；无失败时为 `None`。
pub fn lan_server_last_error() -> Option<String> {
    LAN_LAST_ERROR.lock().unwrap().clone()
}

/// 强制重置后重新尝试启动（供看门狗/自测使用）。
pub fn request_lan_server_restart(coordinator: Arc<Coordinator>) {
    if lan_server_is_running() {
        return;
    }
    if LAN_STATE.load(Ordering::Relaxed) == LAN_STATE_STARTING {
        return;
    }
    if let Some(tx) = LAN_SHUTDOWN.lock().unwrap().take() {
        let _ = tx.send(());
    }
    LAN_STATE.store(LAN_STATE_IDLE, Ordering::SeqCst);
    ensure_started(coordinator);
}

/// 自测用：把 LAN 服务标记为“丢失”，让看门狗/手动重启路径走一遍。
pub fn simulate_lan_server_loss() {
    if let Some(tx) = LAN_SHUTDOWN.lock().unwrap().take() {
        let _ = tx.send(());
    }
    LAN_STATE.store(LAN_STATE_IDLE, Ordering::SeqCst);
    *LAN_LAST_ERROR.lock().unwrap() = Some("自测：模拟 LAN 服务丢失".to_string());
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    coordinator: Arc<Coordinator>,
    registry: SessionRegistry,
    start_gate: Arc<AsyncMutex<()>>,
) -> Result<(), String> {
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| format!("websocket handshake: {e}"))?;
    log::info!("[lan-remote] connected: {addr}");

    let mut session: Option<ActiveSession> = None;
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                let _ = changed;
                log::warn!("[lan-remote] connection {addr} closed by heartbeat sweeper");
                break;
            }
            frame = ws.next() => {
                let Some(frame) = frame else {
                    break;
                };
                let frame = frame.map_err(|e| format!("websocket read: {e}"))?;
                match frame {
                    Message::Text(text) => {
                        let parsed: Result<ClientCommand, _> = serde_json::from_str(text.as_str());
                        let command = match parsed {
                            Ok(cmd) => cmd,
                            Err(error) => {
                                send_json(&mut ws, error_msg("invalid command", &format!("invalid command: {error}"))).await?;
                                continue;
                            }
                        };
                        let command_client_id = command_client_id(&command, addr);
                        match command.kind.as_str() {
                            "hello" => {
                                if let Some(version) = command.protocol_version {
                                    if version > PROTOCOL_VERSION {
                                        send_json(
                                            &mut ws,
                                            error_msg(
                                                "PROTOCOL_MISMATCH",
                                                &format!("客户端协议 v{version} 高于服务端 v{PROTOCOL_VERSION}，请升级手机端"),
                                            ),
                                        )
                                        .await?;
                                        continue;
                                    }
                                }
                                send_json(
                                    &mut ws,
                                    serde_json::json!({
                                        "type": "hello_ok",
                                        "protocolVersion": PROTOCOL_VERSION
                                    }),
                                )
                                .await?;
                            }
                            "start" => {
                                if let Some(active) = session.as_ref() {
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "started",
                                            "sessionId": active.session_id,
                                            "alreadyActive": true
                                        }),
                                    )
                                    .await?;
                                    continue;
                                }
                                let _gate = start_gate.lock().await;
                                let duplicate = {
                                    let reg = registry.lock().unwrap();
                                    reg.values().find(|r| r.owner_client_id == command_client_id).map(
                                        |r| {
                                            (
                                                r.session_id.clone(),
                                                r.previous_first_id.clone(),
                                                r.translation,
                                            )
                                        },
                                    )
                                };
                                if let Some((session_id, previous_first_id, translation)) = duplicate {
                                    session = Some(ActiveSession {
                                        session_id: session_id.clone(),
                                        previous_first_id,
                                        translation,
                                    });
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "started",
                                            "sessionId": session_id,
                                            "alreadyActive": true
                                        }),
                                    )
                                    .await?;
                                    continue;
                                }
                                let blocked = {
                                    let reg = registry.lock().unwrap();
                                    reg.values().next().map(|r| {
                                        (
                                            r.session_id.clone(),
                                            r.owner_client_id.clone(),
                                        )
                                    })
                                };
                                if let Some((session_id, owner_client_id)) = blocked {
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "error",
                                            "code": "SESSION_BLOCKED",
                                            "message": "另一个桌面客户端正在使用当前会话",
                                            "ownerClientId": owner_client_id,
                                            "sessionId": session_id
                                        }),
                                    )
                                    .await?;
                                    continue;
                                }
                                if let Err(error) =
                                    crate::android::native_bridge::promote_remote_recording()
                                {
                                    log::warn!("[lan-remote] promote foreground service failed: {error}");
                                }
                                let previous_first_id =
                                    coordinator.last_finished_session().map(|s| s.id);
                                coordinator.set_remote_capture_mode(true);
                                let translation = command.translation;
                                let start_result = if translation {
                                    coordinator.start_dictation_with_translation().await
                                } else {
                                    coordinator.start_dictation().await
                                };
                                if let Err(error) = start_result {
                                    coordinator.set_remote_capture_mode(false);
                                    send_json(
                                        &mut ws,
                                        error_msg("START_FAILED", &format!("手机端启动听写失败：{error}")),
                                    )
                                    .await?;
                                    continue;
                                }
                                let session_id = Uuid::new_v4().to_string();
                                {
                                    let mut reg = registry.lock().unwrap();
                                    reg.insert(
                                        session_id.clone(),
                                        SessionRecord {
                                            session_id: session_id.clone(),
                                            owner_client_id: command_client_id.clone(),
                                            owner_addr: addr,
                                            previous_first_id: previous_first_id.clone(),
                                            translation,
                                            last_heartbeat: Instant::now(),
                                            phase: SessionPhase::Recording,
                                            result: None,
                                            shutdown_tx: shutdown_tx.clone(),
                                        },
                                    );
                                }
                                session = Some(ActiveSession {
                                    session_id: session_id.clone(),
                                    previous_first_id,
                                    translation,
                                });
                                log::info!(
                                    "[lan-remote] session {session_id} started by client {command_client_id} ({addr})"
                                );
                                send_json(
                                    &mut ws,
                                    serde_json::json!({
                                        "type": "started",
                                        "sessionId": session_id
                                    }),
                                )
                                .await?;
                            }
                            "stop" => {
                                let Some(active) = session.as_ref() else {
                                    send_json(
                                        &mut ws,
                                        error_msg("NO_SESSION", "手机端没有正在进行的会话"),
                                    )
                                    .await?;
                                    continue;
                                };
                                if command.session_id.as_deref() != Some(active.session_id.as_str()) {
                                    send_json(
                                        &mut ws,
                                        error_msg("NO_SESSION", "sessionId 不匹配，手机端没有该会话"),
                                    )
                                    .await?;
                                    continue;
                                }
                                let (already_stopped, is_owner) = {
                                    let reg = registry.lock().unwrap();
                                    match reg.get(&active.session_id) {
                                        Some(record) if record.owner_client_id == command_client_id => {
                                            (record.phase != SessionPhase::Recording, true)
                                        }
                                        _ => (false, false),
                                    }
                                };
                                if !is_owner {
                                    send_json(
                                        &mut ws,
                                        error_msg("SESSION_BLOCKED", "只有持有会话的客户端才能停止"),
                                    )
                                    .await?;
                                    continue;
                                }
                                if already_stopped {
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "stopped",
                                            "sessionId": active.session_id,
                                            "alreadyStopped": true
                                        }),
                                    )
                                    .await?;
                                    continue;
                                }
                                {
                                    let mut reg = registry.lock().unwrap();
                                    if let Some(record) = reg.get_mut(&active.session_id) {
                                        record.phase = SessionPhase::Stopping;
                                    }
                                }
                                coordinator.set_remote_capture_mode(true);
                                let stop_result = if active.translation {
                                    coordinator.stop_dictation_with_translation(true).await
                                } else {
                                    coordinator.stop_dictation().await
                                };
                                if let Err(error) = stop_result {
                                    coordinator.set_remote_capture_mode(false);
                                    remove_record(&registry, &active.session_id);
                                    session = None;
                                    send_json(
                                        &mut ws,
                                        error_msg("STOP_FAILED", &format!("stop failed: {error}")),
                                    )
                                    .await?;
                                    continue;
                                }
                                let text = wait_for_result(
                                    &coordinator,
                                    active.previous_first_id.as_deref(),
                                    RESULT_TIMEOUT,
                                )
                                .await;
                                let Some(text) = text else {
                                    coordinator.set_remote_capture_mode(false);
                                    remove_record(&registry, &active.session_id);
                                    session = None;
                                    send_json(
                                        &mut ws,
                                        error_msg("RESULT_TIMEOUT", "手机端处理超时，未返回听写结果"),
                                    )
                                    .await?;
                                    continue;
                                };
                                let session_id = active.session_id.clone();
                                let result_id = Uuid::new_v4().to_string();
                                let mut sent = 0u8;
                                loop {
                                    {
                                        let mut reg = registry.lock().unwrap();
                                        if let Some(record) = reg.get_mut(&session_id) {
                                            record.phase = SessionPhase::ResultPending;
                                            record.result = Some(PendingResult {
                                                result_id: result_id.clone(),
                                                text: text.clone(),
                                            });
                                        }
                                    }
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "result",
                                            "sessionId": session_id,
                                            "resultId": result_id,
                                            "text": text
                                        }),
                                    )
                                    .await?;
                                    sent += 1;
                                    if sent >= MAX_RESULT_SENDS {
                                        log::warn!(
                                            "[lan-remote] session {} result {result_id} not acked after {sent} sends",
                                            session_id
                                        );
                                        break;
                                    }
                                    match tokio::time::timeout(
                                        ACK_WAIT,
                                        wait_for_ack(&mut ws, &result_id),
                                    )
                                    .await
                                    {
                                        Ok(Ok(())) => break,
                                        Ok(Err(error)) => {
                                            log::warn!(
                                                "[lan-remote] ack read failed for session {}: {error}",
                                                session_id
                                            );
                                            break;
                                        }
                                        Err(_) => {
                                            log::info!(
                                                "[lan-remote] no ack for session {} result {result_id}, resending once",
                                                session_id
                                            );
                                        }
                                    }
                                }
                                coordinator.set_remote_capture_mode(false);
                                remove_record(&registry, &session_id);
                                session = None;
                                send_json(
                                    &mut ws,
                                    serde_json::json!({
                                        "type": "stopped",
                                        "sessionId": session_id
                                    }),
                                )
                                .await?;
                            }
                            "cancel" => {
                                let is_owner = session.as_ref().map(|active| {
                                    registry.lock().unwrap().get(&active.session_id)
                                        .map(|record| record.owner_client_id == command_client_id)
                                        .unwrap_or(false)
                                }).unwrap_or(false);
                                if let Some(active) = session.as_ref() {
                                    if !is_owner {
                                        send_json(
                                            &mut ws,
                                            error_msg("SESSION_BLOCKED", "只有持有会话的客户端才能取消"),
                                        )
                                        .await?;
                                        continue;
                                    }
                                    coordinator.cancel_dictation();
                                    coordinator.set_remote_capture_mode(false);
                                    let session_id = active.session_id.clone();
                                    remove_record(&registry, &session_id);
                                    session = None;
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({
                                            "type": "cancelled",
                                            "sessionId": session_id
                                        }),
                                    )
                                    .await?;
                                } else {
                                    send_json(&mut ws, serde_json::json!({ "type": "cancelled" }))
                                        .await?;
                                }
                            }
                            "ping" => {
                                match command.session_id.as_deref() {
                                    Some(session_id) => {
                                        let accepted = {
                                            let mut reg = registry.lock().unwrap();
                                            reg.get_mut(session_id)
                                                .map(|record| {
                                                    if record.owner_client_id == command_client_id {
                                                        record.last_heartbeat = Instant::now();
                                                        true
                                                    } else {
                                                        false
                                                    }
                                                })
                                                .unwrap_or(false)
                                        };
                                        if accepted {
                                            send_json(
                                                &mut ws,
                                                serde_json::json!({
                                                    "type": "pong",
                                                    "sessionId": session_id
                                                }),
                                            )
                                            .await?;
                                        } else {
                                            send_json(
                                                &mut ws,
                                                error_msg("NO_SESSION", "心跳对应的会话不存在或不属于该客户端"),
                                            )
                                            .await?;
                                        }
                                    }
                                    None => {
                                        // 老客户端/自动发现的轻量 ping：没有 sessionId，只回 pong。
                                        send_json(&mut ws, serde_json::json!({ "type": "pong" }))
                                            .await?;
                                    }
                                }
                            }
                            "status" => {
                                let snapshot = {
                                    let reg = registry.lock().unwrap();
                                    reg.values().next().map(|record| {
                                        (
                                            record.phase.state_name().to_string(),
                                            record.session_id.clone(),
                                            record.owner_client_id.clone(),
                                        )
                                    })
                                };
                                let (state, session_id, owner_client_id) =
                                    snapshot.unwrap_or_else(|| {
                                        ("idle".to_string(), String::new(), String::new())
                                    });
                                send_json(
                                    &mut ws,
                                    serde_json::json!({
                                        "type": "status",
                                        "state": state,
                                        "sessionId": if session_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(session_id) },
                                        "ownerClientId": if owner_client_id.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(owner_client_id) },
                                        "protocolVersion": PROTOCOL_VERSION,
                                        "lastError": null
                                    }),
                                )
                                .await?;
                            }
                            "ack" => {
                                let Some(result_id) = command.result_id.as_deref() else {
                                    send_json(
                                        &mut ws,
                                        error_msg("BAD_ACK", "ack 缺少 resultId"),
                                    )
                                    .await?;
                                    continue;
                                };
                                let matched = {
                                    let mut reg = registry.lock().unwrap();
                                    let mut matched = false;
                                    for record in reg.values_mut() {
                                        if record.owner_client_id == command_client_id {
                                            if let Some(pending) = record.result.as_mut() {
                                                if pending.result_id == result_id {
                                                    record.result = None;
                                                    matched = true;
                                                }
                                            }
                                        }
                                    }
                                    matched
                                };
                                if matched {
                                    send_json(
                                        &mut ws,
                                        serde_json::json!({ "type": "ack_ok" }),
                                    )
                                    .await?;
                                } else {
                                    send_json(
                                        &mut ws,
                                        error_msg("NO_PENDING_RESULT", "没有匹配的待确认结果"),
                                    )
                                    .await?;
                                }
                            }
                            other => {
                                send_json(
                                    &mut ws,
                                    error_msg("UNKNOWN_COMMAND", &format!("unknown command: {other}")),
                                )
                                .await?;
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(payload) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    _ => {}
                }
            }
        }
    }

    // 清理路径：正常断开、被看门狗关闭、错误返回都会走到这里。
    let owned_session_id = session.as_ref().map(|s| s.session_id.clone());
    if let Some(session_id) = owned_session_id {
        let should_cancel = {
            let mut reg = registry.lock().unwrap();
            let owned_by_this_connection = reg
                .get(&session_id)
                .map(|record| record.owner_addr == addr)
                .unwrap_or(false);
            let phase = reg.get(&session_id).map(|record| record.phase);
            if owned_by_this_connection {
                reg.remove(&session_id);
            }
            owned_by_this_connection && phase == Some(SessionPhase::Recording)
        };
        if should_cancel {
            log::warn!("[lan-remote] connection {addr} dropped during recording, cancelling session {session_id}");
            coordinator.cancel_dictation();
        }
    }
    coordinator.set_remote_capture_mode(false);
    log::info!("[lan-remote] disconnected: {addr}");
    Ok(())
}

fn command_client_id(command: &ClientCommand, addr: SocketAddr) -> String {
    command
        .client_id
        .clone()
        .unwrap_or_else(|| format!("legacy:{addr}"))
}

fn remove_record(registry: &SessionRegistry, session_id: &str) {
    registry.lock().unwrap().remove(session_id);
}

/// stop 后等待客户端确认结果；收到匹配的 ack 即成功。
async fn wait_for_ack<S>(ws: &mut S, result_id: &str) -> Result<(), String>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        let Some(frame) = ws.next().await else {
            return Err("connection closed while waiting for ack".to_string());
        };
        let frame = frame.map_err(|e| format!("websocket read while waiting ack: {e}"))?;
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad ack json: {e}"))?;
                match value["type"].as_str() {
                    Some("ack") if value["resultId"].as_str() == Some(result_id) => {
                        return Ok(());
                    }
                    Some("error") => {
                        return Err(value["message"]
                            .as_str()
                            .unwrap_or("phone error while waiting ack")
                            .to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => return Err("connection closed while waiting for ack".to_string()),
            _ => {}
        }
    }
}

fn error_msg(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "code": code,
        "message": message
    })
}

async fn send_json<S>(ws: &mut S, value: serde_json::Value) -> Result<(), String>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    ws.send(Message::Text(value.to_string().into()))
        .await
        .map_err(|e| format!("websocket send: {e}"))
}

/// stop 后轮询历史记录，直到出现一条新会话且 final_text 非空。
async fn wait_for_result(
    coordinator: &Coordinator,
    previous_first_id: Option<&str>,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(session) = coordinator.last_finished_session() {
            let is_new = match previous_first_id {
                Some(previous) => session.id != previous,
                None => true,
            };
            if is_new && !session.final_text.trim().is_empty() {
                return Some(session.final_text);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
