//! Android 局域网远程听写服务。
#![cfg(target_os = "android")]
//!
//! 手机上的 OpenLess 启动后在 `0.0.0.0:45678` 监听 WebSocket。
//! 电脑端（见 `openless-all/remote-client`）通过局域网连接本服务：
//!
//! ```text
//! PC ── ws://<phone-ip>:45678 ──► OpenLess(Android)
//!        {start}                → 开始录音/ASR/润色
//!        {stop}                 → 结束会话，等待 final_text
//!        {result, text}         → 回传最终文本
//! ```
//!
//! 协议（JSON 文本帧，字段 `type`）：
//!
//! 客户端 → 服务端：
//! - `{"type":"start","translation":false}`
//! - `{"type":"stop","translation":false}`
//! - `{"type":"cancel"}`
//! - `{"type":"ping"}`
//!
//! 服务端 → 客户端：
//! - `{"type":"started"}`
//! - `{"type":"stopped"}`
//! - `{"type":"result","text":"..."}`
//! - `{"type":"error","message":"..."}`
//! - `{"type":"pong"}`
//!
//! 多客户端：允许多个桌面客户端同时连接；同一时刻只有一个客户端能持有
//! 活跃听写会话，其他客户端发起 `start` 会收到“连接已被阻塞”错误。
//!
//! 安全说明：这是局域网 PoC，未做认证；请仅在可信网络使用。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::coordinator::Coordinator;

/// 默认监听端口（固定，PoC）。
pub const DEFAULT_PORT: u16 = 45678;

/// 等待最终文本落历史的超时。
const RESULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientCommand {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    translation: bool,
}

/// 当前连接上的活跃会话。`previous_first_id` 用于区分 stop 后新写入的历史记录。
struct ActiveSession {
    previous_first_id: Option<String>,
    translation: bool,
}

/// 启动 LAN 服务。绑定失败只记日志，不 panic。
pub fn start(coordinator: Arc<Coordinator>) {
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("0.0.0.0", DEFAULT_PORT)).await {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("[lan-remote] bind 0.0.0.0:{DEFAULT_PORT} failed: {error}");
                return;
            }
        };
        log::info!("[lan-remote] listening on 0.0.0.0:{DEFAULT_PORT}");
        // 全局会话占用表：同一时刻只允许一个桌面客户端持有活跃听写会话。
        let registry = Arc::new(StdMutex::new(None::<SocketAddr>));
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let coordinator = Arc::clone(&coordinator);
                    let registry = Arc::clone(&registry);
                    tauri::async_runtime::spawn(async move {
                        if let Err(error) =
                            handle_connection(stream, addr, coordinator, registry).await
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
    });
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    coordinator: Arc<Coordinator>,
    registry: Arc<StdMutex<Option<SocketAddr>>>,
) -> Result<(), String> {
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| format!("websocket handshake: {e}"))?;
    log::info!("[lan-remote] connected: {addr}");

    let mut session: Option<ActiveSession> = None;

    loop {
        let Some(frame) = ws.next().await else {
            break;
        };
        let frame = frame.map_err(|e| format!("websocket read: {e}"))?;
        match frame {
            Message::Text(text) => {
                let parsed: Result<ClientCommand, _> = serde_json::from_str(text.as_str());
                let command = match parsed {
                    Ok(cmd) => cmd,
                    Err(error) => {
                        send_json(&mut ws, error_msg(&format!("invalid command: {error}"))).await?;
                        continue;
                    }
                };
                match command.kind.as_str() {
                    "start" => {
                        if session.is_some() {
                            send_json(&mut ws, error_msg("session already active")).await?;
                            continue;
                        }
                        let blocked = {
                            let mut owner = registry.lock().unwrap();
                            match *owner {
                                Some(current) if current != addr => true,
                                None => {
                                    *owner = Some(addr);
                                    false
                                }
                                _ => false,
                            }
                        };
                        if blocked {
                            send_json(
                                &mut ws,
                                error_msg("连接已被阻塞：另一个桌面客户端正在使用当前会话"),
                            )
                            .await?;
                            continue;
                        }
                        if let Err(error) = crate::android::native_bridge::promote_remote_recording()
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
                            release_owner(&registry, addr);
                            send_json(
                                &mut ws,
                                error_msg(&format!("手机端启动听写失败：{error}")),
                            )
                            .await?;
                            continue;
                        }
                        session = Some(ActiveSession {
                            previous_first_id,
                            translation,
                        });
                        send_json(&mut ws, serde_json::json!({ "type": "started" })).await?;
                    }
                    "stop" => {
                        let Some(active) = session.take() else {
                            send_json(&mut ws, error_msg("手机端没有正在进行的会话")).await?;
                            continue;
                        };
                        release_owner(&registry, addr);
                        coordinator.set_remote_capture_mode(true);
                        let stop_result = if active.translation {
                            coordinator.stop_dictation_with_translation(true).await
                        } else {
                            coordinator.stop_dictation().await
                        };
                        if let Err(error) = stop_result {
                            coordinator.set_remote_capture_mode(false);
                            send_json(&mut ws, error_msg(&format!("stop failed: {error}"))).await?;
                            continue;
                        }
                        let text = wait_for_result(
                            &coordinator,
                            active.previous_first_id.as_deref(),
                            RESULT_TIMEOUT,
                        )
                        .await;
                        match text {
                            Some(text) => {
                                send_json(
                                    &mut ws,
                                    serde_json::json!({ "type": "result", "text": text }),
                                )
                                .await?;
                            }
                            None => {
                                send_json(
                                    &mut ws,
                                    error_msg("手机端处理超时，未返回听写结果"),
                                )
                                .await?;
                            }
                        }
                        coordinator.set_remote_capture_mode(false);
                        send_json(&mut ws, serde_json::json!({ "type": "stopped" })).await?;
                    }
                    "cancel" => {
                        release_owner(&registry, addr);
                        coordinator.cancel_dictation();
                        coordinator.set_remote_capture_mode(false);
                        send_json(&mut ws, serde_json::json!({ "type": "cancelled" })).await?;
                    }
                    "ping" => {
                        send_json(&mut ws, serde_json::json!({ "type": "pong" })).await?;
                    }
                    other => {
                        send_json(
                            &mut ws,
                            error_msg(&format!("unknown command: {other}")),
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

    coordinator.set_remote_capture_mode(false);
    release_owner(&registry, addr);
    log::info!("[lan-remote] disconnected: {addr}");
    Ok(())
}

/// 释放该连接持有的全局会话占用（仅当占用者确实是该连接时）。
fn release_owner(registry: &StdMutex<Option<SocketAddr>>, addr: SocketAddr) {
    let mut owner = registry.lock().unwrap();
    if *owner == Some(addr) {
        *owner = None;
    }
}

fn error_msg(message: &str) -> serde_json::Value {
    serde_json::json!({ "type": "error", "message": message })
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
