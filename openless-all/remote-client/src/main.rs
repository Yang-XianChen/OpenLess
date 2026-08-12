//! OpenLess 局域网远程听写 —— 极简无界面电脑端。
//!
//! 用法：
//! ```text
//! openless-remote-client --address 192.168.1.20:45678 [--hotkey "RightAlt"] [--fcitx]
//! openless-remote-client --auto-discover [--hotkey "RightAlt"] [--fcitx]
//! ```
//!
//! 按下热键：向手机 OpenLess 发送 `start`，手机开始录音/ASR/润色；
//! 松开（或切换模式下再按一次）：发送 `stop`，手机回传 `final_text`，
//! 电脑端优先通过 fcitx5 `CommitText` 插入光标（Wayland 可用）。
//!
//! fcitx5 通道下会在输入法候选/提示区显示状态：
//! 正在录音 → 正在转录 → 完成清除；连接失败会立即显示提示并在数秒后自动消失。

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clap::Parser;
use fs2::FileExt;
use serde_json::json;
use tungstenite::Message;
use uuid::Uuid;

type ClientSocket = tungstenite::WebSocket<std::net::TcpStream>;

static SESSION: OnceLock<Mutex<Option<SessionHandle>>> = OnceLock::new();
static DISCOVERED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// 连接断开后，下次后台扫描自动发现手机时弹一次「已自动连接」提示。
static CONNECTED_AGAIN_PENDING: AtomicBool = AtomicBool::new(false);
/// 提示代际：新的 SetAuxDown 会让旧 flash 不再清掉新提示。
static AUX_TOKEN: AtomicU64 = AtomicU64::new(0);

const DEFAULT_PORT: u16 = 45678;
const PROTOCOL_VERSION: u32 = 2;
const AUX_RECORDING: &str = "🎤 正在录音…";
const AUX_TRANSCRIBING: &str = "⏳ 正在转录…";
const AUX_RETRY: &str = "连接失败，正在重试…";
const ERR_NO_DEVICE: &str = "未连接手机：请确认手机端 OpenLess 已运行，等待后台扫描发现设备";
const SCAN_INTERVAL: Duration = Duration::from_secs(15);
/// 会话期间的心跳间隔（服务端超时 3 秒）。
const SESSION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// 空闲状态下对已知手机的轻量存活探测间隔。
const DISCOVERY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_HEARTBEAT_FAIL_LIMIT: u32 = 2;
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
const PROBE_READ_TIMEOUT: Duration = Duration::from_millis(1200);
const SESSION_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 连接后等待 `hello_ok` 的最长时间；老服务端不认识 hello 会返回 error 或保持沉默。
const HELLO_ACK_TIMEOUT: Duration = Duration::from_millis(800);
/// 单实例锁文件。
const SINGLE_INSTANCE_LOCK: &str = "/tmp/openless-remote-client.lock";

/// 活跃会话的进程内句柄。真正的 WebSocket 由独立线程持有。
#[derive(Clone)]
struct SessionHandle {
    address: String,
    command_tx: mpsc::Sender<SessionCommand>,
}

enum SessionCommand {
    Stop,
}

enum SessionEvent {
    Stopped,
    Cancelled,
    Failed(String),
    Lost(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Idle,
    Recording,
    Stopping,
}

#[derive(Parser, Debug)]
#[command(
    name = "openless-remote-client",
    about = "Headless OpenLess Android LAN dictation client"
)]
struct Args {
    /// 手机地址，例如 192.168.1.20:45678（与 --auto-discover 二选一）
    #[arg(long)]
    address: Option<String>,
    /// 自动搜索局域网内监听 45678 端口的手机
    #[arg(long)]
    auto_discover: bool,
    /// 触发热键，例如 Ctrl+Shift+Space
    #[arg(long, default_value = "Ctrl+Shift+Space")]
    hotkey: String,
    /// 切换模式：按一下开始，再按一下结束（默认按住说话模式）
    #[arg(long)]
    toggle: bool,
    /// 使用 fcitx5 DBus 热键/输入法通道（Wayland 下捕获右 Alt 需要）
    #[arg(long)]
    fcitx: bool,
    /// 可选配置文件（未实现，仅保留占位）
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    let _ = SESSION.set(Mutex::new(None));
    let _ = DISCOVERED.set(Mutex::new(None));

    if !args.fcitx {
        eprintln!("[remote] 此构建仅支持 fcitx5 通道（Wayland 下请加 --fcitx）");
        std::process::exit(1);
    }
    if let Err(error) = acquire_single_instance_lock() {
        eprintln!("[remote] {error}");
        std::process::exit(1);
    }
    let client_id = Uuid::new_v4().to_string();
    run_fcitx(
        args.address.clone(),
        args.auto_discover,
        args.toggle,
        client_id,
    );
}

/// 单实例锁：第二个实例直接退出，避免系统服务之外再开一个客户端抢占热键。
fn acquire_single_instance_lock() -> Result<std::fs::File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(SINGLE_INSTANCE_LOCK)
        .map_err(|error| format!("无法打开单实例锁文件 {SINGLE_INSTANCE_LOCK}: {error}"))?;
    FileExt::try_lock_exclusive(&file)
        .map_err(|_| "另一个 openless-remote-client 实例已在运行".to_string())?;
    Ok(file)
}

// ───────────────────────── fcitx5 通道 ─────────────────────────

const DBUS_DEST: &str = "org.fcitx.Fcitx5";
const DBUS_PATH: &str = "/openless";
const DBUS_IFACE: &str = "org.fcitx.Fcitx.OpenLess1";
const KEYSYM_ALT_R: u32 = 0xffea;

/// Wayland 方案：通过 fcitx5 OpenLess 插件监听右 Alt 键事件，
/// 并在输入法提示区显示录音/转录/失败状态。
fn run_fcitx(address: Option<String>, auto_discover: bool, toggle: bool, client_id: String) -> ! {
    use dbus::blocking::SyncConnection;

    let conn = match SyncConnection::new_session() {
        Ok(conn) => conn,
        Err(error) => {
            eprintln!("[remote] DBus session failed: {error}");
            std::process::exit(1);
        }
    };

    let rule = match dbus::message::MatchRule::parse(
        "type='signal',interface='org.fcitx.Fcitx.OpenLess1'",
    ) {
        Ok(rule) => rule,
        Err(error) => {
            eprintln!("[remote] invalid DBus match rule: {error}");
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::channel::<bool>();
    if let Err(error) = conn.add_match(rule, move |args: (u32, u32, bool), _conn, msg| {
        let member: String = msg
            .member()
            .as_ref()
            .map(|m| m.to_string())
            .unwrap_or_default();
        if member == "DictationKeyEvent" {
            let _ = tx.send(args.2);
        }
        true
    }) {
        eprintln!("[remote] failed to add DBus match: {error}");
        std::process::exit(1);
    }

    // 初始同步：等 fcitx5 可用后把触发键设为右 Alt。
    loop {
        if fcitx5_available(&conn) {
            match set_hotkey_raw(&conn, KEYSYM_ALT_R, 0) {
                Ok(()) => break,
                Err(error) => {
                    eprintln!("[remote] SetHotkeyRaw failed: {error}");
                    std::process::exit(1);
                }
            }
        }
        let _ = conn.process(Duration::from_millis(500));
    }

    println!(
        "[remote] fcitx5 Right Alt active ({})",
        if toggle { "toggle" } else { "hold to talk" }
    );
    if auto_discover {
        start_background_scanner();
    }
    let (event_tx, event_rx) = mpsc::channel::<SessionEvent>();
    let mut state = ClientState::Idle;
    let mut session_address: Option<String> = None;
    loop {
        let _ = conn.process(Duration::from_millis(200));
        while let Ok(event) = event_rx.try_recv() {
            state = handle_session_event(event, &mut session_address);
        }
        while let Ok(is_press) = rx.try_recv() {
            let should_start = is_press && state == ClientState::Idle;
            let should_stop = !is_press && !toggle && state == ClientState::Recording
                || is_press && toggle && state == ClientState::Recording;
            if should_start {
                match start_flow(
                    address.as_deref(),
                    auto_discover,
                    event_tx.clone(),
                    &client_id,
                ) {
                    Ok(found) => {
                        session_address = Some(found);
                        state = ClientState::Recording;
                    }
                    Err(error) => eprintln!("[remote] start failed: {error}"),
                }
            } else if should_stop {
                state = ClientState::Stopping;
                if let Some(address) = session_address.take() {
                    stop_flow(&address);
                }
            }
        }
    }
}

fn fcitx5_available(conn: &dbus::blocking::SyncConnection) -> bool {
    use dbus::blocking::BlockingSender;
    let msg = match dbus::Message::new_method_call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
    ) {
        Ok(msg) => msg,
        Err(_) => return false,
    };
    let msg = msg.append1("org.fcitx.Fcitx5");
    match conn.send_with_reply_and_block(msg, Duration::from_secs(1)) {
        Ok(reply) => reply.read1::<bool>().unwrap_or(false),
        Err(_) => false,
    }
}

fn set_hotkey_raw(
    conn: &dbus::blocking::SyncConnection,
    sym: u32,
    states: u32,
) -> Result<(), String> {
    use dbus::blocking::BlockingSender;
    let msg = dbus::Message::new_method_call(DBUS_DEST, DBUS_PATH, DBUS_IFACE, "SetHotkeyRaw")
        .map_err(|e| format!("build msg: {e}"))?
        .append2(sym, states);
    conn.send_with_reply_and_block(msg, Duration::from_secs(3))
        .map_err(|e| format!("SetHotkeyRaw: {e}"))?;
    Ok(())
}

// ───────────────────────── 会话控制 ─────────────────────────

fn handle_session_event(event: SessionEvent, session_address: &mut Option<String>) -> ClientState {
    match event {
        SessionEvent::Stopped => {
            println!("[remote] session stopped");
            clear_active_session();
            *session_address = None;
            let _ = clear_aux_down();
            ClientState::Idle
        }
        SessionEvent::Cancelled => {
            println!("[remote] session cancelled");
            clear_active_session();
            *session_address = None;
            let _ = clear_aux_down();
            ClientState::Idle
        }
        SessionEvent::Failed(message) => {
            clear_active_session();
            *session_address = None;
            flash_aux(&format!("❌ {message}"));
            ClientState::Idle
        }
        SessionEvent::Lost(message) => {
            println!("[remote] session lost: {message}");
            clear_active_session();
            *session_address = None;
            CONNECTED_AGAIN_PENDING.store(true, Ordering::Relaxed);
            flash_aux(&format!("❌ {message}"));
            ClientState::Idle
        }
    }
}

fn clear_active_session() {
    if let Some(session) = SESSION.get() {
        *session.lock().unwrap() = None;
    }
}

fn start_flow(
    address: Option<&str>,
    auto_discover: bool,
    event_tx: mpsc::Sender<SessionEvent>,
    client_id: &str,
) -> Result<String, String> {
    let resolved = match resolve_address(address, auto_discover) {
        Ok(resolved) => resolved,
        Err(error) => {
            CONNECTED_AGAIN_PENDING.store(true, Ordering::Relaxed);
            flash_aux(&format!("❌ {error}"));
            return Err(error);
        }
    };

    let mut last_error = "未连接".to_string();
    for attempt in 0..2 {
        println!("[remote] connecting {resolved} (attempt {})", attempt + 1);
        match start_session(&resolved, client_id) {
            Ok((socket, session_id)) => {
                if let Some(cache) = DISCOVERED.get() {
                    *cache.lock().unwrap() = Some(resolved.clone());
                }
                let (command_tx, command_rx) = mpsc::channel::<SessionCommand>();
                let thread_event_tx = event_tx.clone();
                let thread_client_id = client_id.to_string();
                let thread_address = resolved.clone();
                std::thread::spawn(move || {
                    session_loop(
                        socket,
                        thread_address,
                        thread_client_id,
                        session_id,
                        command_rx,
                        thread_event_tx,
                    );
                });
                *SESSION
                    .get()
                    .expect("SESSION initialized in main")
                    .lock()
                    .unwrap() = Some(SessionHandle {
                    address: resolved.clone(),
                    command_tx,
                });
                let _ = set_aux_down(AUX_RECORDING);
                return Ok(resolved);
            }
            Err(error) => {
                last_error = error;
                if attempt == 0 {
                    let _ = set_aux_down(AUX_RETRY);
                }
            }
        }
    }

    if let Some(cache) = DISCOVERED.get() {
        let mut guard = cache.lock().unwrap();
        if guard.as_deref() == Some(resolved.as_str()) {
            *guard = None;
        }
    }
    CONNECTED_AGAIN_PENDING.store(true, Ordering::Relaxed);
    flash_aux(&format!("❌ 连接失败：{last_error}"));
    Err(last_error)
}

fn stop_flow(address: &str) {
    let _ = set_aux_down(AUX_TRANSCRIBING);
    let handle = SESSION
        .get()
        .and_then(|session| session.lock().unwrap().clone());
    match handle {
        Some(handle) if handle.address == address => {
            if handle.command_tx.send(SessionCommand::Stop).is_err() {
                clear_active_session();
                flash_aux("❌ 转录失败：会话线程已退出");
            }
        }
        Some(_) => flash_aux("❌ 转录失败：当前地址与会话不一致"),
        None => flash_aux("❌ 转录失败：没有活动会话"),
    }
}

fn resolve_address(address: Option<&str>, auto_discover: bool) -> Result<String, String> {
    if let Some(address) = address {
        let address = address.trim();
        if !address.is_empty() {
            return Ok(address.to_string());
        }
    }
    if auto_discover {
        if let Some(cache) = DISCOVERED.get() {
            if let Some(found) = cache.lock().unwrap().clone() {
                return Ok(found);
            }
        }
        Err(ERR_NO_DEVICE.to_string())
    } else {
        Err("未配置手机地址（--address 或 --auto-discover）".to_string())
    }
}

/// 后台扫描策略：
/// - 已缓存到手机地址后不再重复扫描；
/// - 已连接时每 DISCOVERY_HEARTBEAT_INTERVAL 秒对已知手机做一次轻量心跳；
/// - 连续 DISCOVERY_HEARTBEAT_FAIL_LIMIT 次心跳失败（连接断开）才清除缓存并重新扫描；
/// - 未发现设备时每 SCAN_INTERVAL 秒重试一次。
fn start_background_scanner() {
    std::thread::spawn(|| {
        let mut last: Option<String> = None;
        let mut last_heartbeat = Instant::now();
        let mut heartbeat_failures: u32 = 0;
        loop {
            // 会话进行中不做任何扫描/心跳，避免干扰手机。
            if let Some(session) = SESSION.get() {
                if session.lock().unwrap().is_some() {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }

            // 已连上/已知设备：不整段扫描，只做轻量心跳。
            let known = DISCOVERED
                .get()
                .and_then(|cache| cache.lock().unwrap().clone());
            if let Some(known) = known {
                if last_heartbeat.elapsed() >= DISCOVERY_HEARTBEAT_INTERVAL {
                    last_heartbeat = Instant::now();
                    if ping_phone(&known) {
                        heartbeat_failures = 0;
                    } else {
                        heartbeat_failures += 1;
                        if heartbeat_failures >= DISCOVERY_HEARTBEAT_FAIL_LIMIT {
                            println!("[remote] heartbeat lost: {known}");
                            CONNECTED_AGAIN_PENDING.store(true, Ordering::Relaxed);
                            flash_aux("❌ 手机连接断开，正在重新扫描…");
                            if let Some(cache) = DISCOVERED.get() {
                                let mut guard = cache.lock().unwrap();
                                if guard.as_deref() == Some(known.as_str()) {
                                    *guard = None;
                                }
                            }
                            heartbeat_failures = 0;
                            last = None;
                            continue;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }

            let found = discover_phone(DEFAULT_PORT);
            if found != last {
                match &found {
                    Some(found) => {
                        println!("[remote] background discovery: {found}");
                        if CONNECTED_AGAIN_PENDING.swap(false, Ordering::Relaxed) {
                            flash_aux(&format!("✅ 已自动连接手机 {found}"));
                        }
                    }
                    None => println!("[remote] background discovery: no phone found"),
                }
                last = found.clone();
            }
            if let Some(cache) = DISCOVERED.get() {
                *cache.lock().unwrap() = found.clone();
            }
            if found.is_some() {
                last_heartbeat = Instant::now();
                std::thread::sleep(Duration::from_secs(1));
            } else {
                std::thread::sleep(SCAN_INTERVAL);
            }
        }
    });
}

/// 扫描本机局域网私有网段里监听指定端口并能回复 OpenLess ping 的设备。
/// 每个探测都带显式连接/读取超时，且任意一个 pong 命中即立即返回，
/// 不再等待所有探测线程结束。
fn discover_phone(port: u16) -> Option<String> {
    let prefixes = local_private_prefixes();
    if prefixes.is_empty() {
        return None;
    }

    let found = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();
    for prefix in prefixes {
        for host in 1..=254 {
            let tx = tx.clone();
            let found = Arc::clone(&found);
            let host = format!("{prefix}.{host}");
            std::thread::spawn(move || {
                if found.load(Ordering::Relaxed) {
                    return;
                }
                probe_phone(&host, port, &tx, &found);
            });
        }
    }
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

fn probe_phone(host: &str, port: u16, tx: &mpsc::Sender<String>, found: &AtomicBool) {
    let address = format!("{host}:{port}");
    if ping_phone(&address) && !found.swap(true, Ordering::Relaxed) {
        let _ = tx.send(address);
    }
}

/// 对单个手机地址做一次轻量 WebSocket ping，收到 pong 视为存活。
fn ping_phone(address: &str) -> bool {
    let Ok(socket_addr) = address.parse::<SocketAddr>() else {
        return false;
    };
    let Ok(tcp) = std::net::TcpStream::connect_timeout(&socket_addr, PROBE_CONNECT_TIMEOUT) else {
        return false;
    };
    let _ = tcp.set_read_timeout(Some(PROBE_READ_TIMEOUT));
    let _ = tcp.set_nodelay(true);
    let Ok((mut socket, _)) = tungstenite::client::client(format!("ws://{address}"), tcp) else {
        return false;
    };
    if socket
        .send(Message::Text(json!({ "type": "ping" }).to_string().into()))
        .is_err()
    {
        return false;
    }
    matches!(socket.read(), Ok(Message::Text(text)) if text.contains("\"pong\""))
}

fn local_private_prefixes() -> Vec<String> {
    let mut prefixes = Vec::new();
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in interfaces {
            if let IpAddr::V4(v4) = ip {
                if v4.is_loopback() || v4.is_link_local() {
                    continue;
                }
                if !interface_up(&name) {
                    continue;
                }
                let octets = v4.octets();
                let private = octets[0] == 10
                    || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                    || (octets[0] == 192 && octets[1] == 168);
                if private {
                    prefixes.push(format!("{}.{}.{}", octets[0], octets[1], octets[2]));
                }
            }
        }
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// Linux 下只扫描状态为 up 的接口（跳过 docker0 等未启用的网桥）。
#[cfg(target_os = "linux")]
fn interface_up(name: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
        .map(|state| state.trim() == "up")
        .unwrap_or(true)
}

#[cfg(not(target_os = "linux"))]
fn interface_up(_name: &str) -> bool {
    true
}

/// 连接手机并完成 `hello` 握手 + `start`。返回会话 socket 与服务端生成的
/// `sessionId`（老服务端没有该字段时为 `None`，客户端进入兼容模式）。
fn start_session(address: &str, client_id: &str) -> Result<(ClientSocket, Option<String>), String> {
    let url = format!("ws://{address}");
    let socket_addr = address
        .parse::<SocketAddr>()
        .map_err(|e| format!("bad address {address}: {e}"))?;
    let tcp = std::net::TcpStream::connect_timeout(&socket_addr, PROBE_CONNECT_TIMEOUT)
        .map_err(|e| format!("connect {url}: {e}"))?;
    let _ = tcp.set_read_timeout(Some(SESSION_READ_TIMEOUT));
    let _ = tcp.set_nodelay(true);
    let (mut socket, _response) =
        tungstenite::client::client(&url, tcp).map_err(|e| format!("handshake {url}: {e}"))?;

    // 协议握手：新服务端回 hello_ok；老服务端回 unknown command error 或沉默。
    send_json(
        &mut socket,
        json!({
            "type": "hello",
            "clientId": client_id,
            "protocolVersion": PROTOCOL_VERSION
        }),
    )?;
    let hello_deadline = Instant::now() + HELLO_ACK_TIMEOUT;
    let mut hello_ok = false;
    loop {
        if Instant::now() >= hello_deadline {
            break;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad hello ack json: {e}"))?;
                match value["type"].as_str() {
                    Some("hello_ok") => {
                        hello_ok = true;
                        break;
                    }
                    Some("error") => {
                        // 老服务端不认识 hello：保留错误已消费，继续用 start 兼容。
                        break;
                    }
                    _ => continue,
                }
            }
            Ok(Message::Close(_)) => {
                return Err("phone closed connection during hello handshake".to_string());
            }
            Ok(_) => {}
            Err(error) if is_read_timeout(&error) => continue,
            Err(error) => return Err(format!("hello handshake read: {error}")),
        }
    }
    send_json(
        &mut socket,
        json!({
            "type": "start",
            "clientId": client_id,
            "translation": false,
            "protocolVersion": PROTOCOL_VERSION
        }),
    )?;
    let deadline = Instant::now() + SESSION_READ_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for phone started".to_string());
        }
        let frame = match socket.read() {
            Ok(frame) => frame,
            Err(error) if is_read_timeout(&error) => continue,
            Err(error) => return Err(format!("read started ack: {error}")),
        };
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad started ack json: {e}"))?;
                match value["type"].as_str() {
                    Some("started") => {
                        let session_id = value["sessionId"].as_str().map(str::to_string);
                        if hello_ok && session_id.is_none() {
                            return Err("手机端协议握手成功但 start 未返回 sessionId".to_string());
                        }
                        return Ok((socket, session_id));
                    }
                    Some("error") => {
                        let mut message = value["message"]
                            .as_str()
                            .unwrap_or("phone error")
                            .to_string();
                        if value["code"].as_str() == Some("SESSION_BLOCKED") {
                            if let Some(owner) = value["ownerClientId"].as_str() {
                                message.push_str(&format!("（占用者 {owner}）"));
                            }
                        }
                        return Err(message);
                    }
                    _ => continue,
                }
            }
            Message::Close(_) => return Err("phone closed connection during start".to_string()),
            _ => {}
        }
    }
}

/// 会话线程：录音期间每 1 秒心跳，读取服务端消息；收到 stop/cancel 命令后执行对应流程。
fn session_loop(
    mut socket: ClientSocket,
    address: String,
    client_id: String,
    session_id: Option<String>,
    command_rx: mpsc::Receiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    let _ = socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(300)));
    let mut last_heartbeat = Instant::now();
    let mut seen_results = HashSet::new();
    loop {
        match command_rx.try_recv() {
            Ok(SessionCommand::Stop) => {
                let outcome = stop_session_io(
                    &mut socket,
                    &address,
                    &client_id,
                    session_id.as_deref(),
                    &mut seen_results,
                );
                let _ = event_tx.send(match outcome {
                    Ok(()) => SessionEvent::Stopped,
                    Err(error) => SessionEvent::Failed(format!("转录失败：{error}")),
                });
                return;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }

        if let Some(session_id) = session_id.as_deref() {
            if last_heartbeat.elapsed() >= SESSION_HEARTBEAT_INTERVAL {
                if send_json(
                    &mut socket,
                    json!({
                        "type": "ping",
                        "clientId": client_id,
                        "sessionId": session_id
                    }),
                )
                .is_err()
                {
                    let _ = event_tx.send(SessionEvent::Lost(
                        "心跳发送失败，手机端即将释放会话".to_string(),
                    ));
                    return;
                }
                last_heartbeat = Instant::now();
            }
        }

        match socket.read() {
            Ok(Message::Text(text)) => {
                let value: serde_json::Value = match serde_json::from_str(text.as_str()) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                match value["type"].as_str() {
                    Some("pong") => {}
                    Some("error") => {
                        let message = value["message"]
                            .as_str()
                            .unwrap_or("手机端错误")
                            .to_string();
                        let _ = event_tx.send(SessionEvent::Lost(message));
                        return;
                    }
                    Some("stopped") => {
                        let _ = event_tx.send(SessionEvent::Stopped);
                        return;
                    }
                    Some("cancelled") => {
                        let _ = event_tx.send(SessionEvent::Cancelled);
                        return;
                    }
                    Some("result") => {
                        // 录音中不应出现 result；仅记录 resultId 防后续重复。
                        if let Some(result_id) = value["resultId"].as_str() {
                            seen_results.insert(result_id.to_string());
                        }
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => {
                let _ = event_tx.send(SessionEvent::Lost("手机端断开连接".to_string()));
                return;
            }
            Ok(_) => {}
            Err(error) if is_read_timeout(&error) => {}
            Err(error) => {
                let _ = event_tx.send(SessionEvent::Lost(format!("连接中断：{error}")));
                return;
            }
        }
    }
}

/// 在会话线程内执行 stop：发送 stop，等待 result，插入文本，回传 ack。
fn stop_session_io(
    socket: &mut ClientSocket,
    address: &str,
    client_id: &str,
    session_id: Option<&str>,
    seen_results: &mut HashSet<String>,
) -> Result<(), String> {
    let stop = match session_id {
        Some(session_id) => json!({
            "type": "stop",
            "clientId": client_id,
            "sessionId": session_id,
            "translation": false
        }),
        None => json!({ "type": "stop", "translation": false }),
    };
    send_json(socket, stop)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if Instant::now() >= deadline {
            if let Some(session_id) = session_id {
                let _ = send_json(
                    socket,
                    json!({
                        "type": "cancel",
                        "clientId": client_id,
                        "sessionId": session_id
                    }),
                );
            }
            return Err("timed out waiting for phone result".to_string());
        }
        let frame = match socket.read() {
            Ok(frame) => frame,
            Err(error) if is_read_timeout(&error) => {
                if Instant::now() >= deadline {
                    return Err("timed out waiting for phone result".to_string());
                }
                continue;
            }
            Err(error) => return Err(format!("read from {address}: {error}")),
        };
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad json from phone: {e}"))?;
                match value["type"].as_str() {
                    Some("result") => {
                        let result_id = value["resultId"].as_str().unwrap_or_default().to_string();
                        if !result_id.is_empty() && !seen_results.insert(result_id.clone()) {
                            continue; // 重复 result，跳过防二次粘贴
                        }
                        let text = value["text"].as_str().unwrap_or_default().to_string();
                        if text.is_empty() {
                            return Err("phone returned empty text".to_string());
                        }
                        insert_text(&text)?;
                        if session_id.is_some() && !result_id.is_empty() {
                            let _ = send_json(
                                socket,
                                json!({
                                    "type": "ack",
                                    "clientId": client_id,
                                    "resultId": result_id
                                }),
                            );
                        }
                        return Ok(());
                    }
                    Some("error") => {
                        return Err(value["message"]
                            .as_str()
                            .unwrap_or("phone error")
                            .to_string());
                    }
                    Some("stopped") => {
                        return Err("phone stopped without result".to_string());
                    }
                    Some("cancelled") => {
                        return Err("session cancelled by phone".to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => return Err("phone closed connection before result".to_string()),
            _ => {}
        }
    }
}

/// 判断错误是否来自 socket 读超时（用于在截止时间内继续等待）。
fn is_read_timeout(error: &tungstenite::Error) -> bool {
    match error {
        tungstenite::Error::Io(io) => {
            io.kind() == std::io::ErrorKind::WouldBlock || io.kind() == std::io::ErrorKind::TimedOut
        }
        _ => false,
    }
}

fn send_json(socket: &mut ClientSocket, value: serde_json::Value) -> Result<(), String> {
    socket
        .send(Message::Text(value.to_string().into()))
        .map_err(|e| format!("send: {e}"))
}

// ───────────────────────── 插入 ─────────────────────────

fn insert_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        match commit_text_via_fcitx(text) {
            Ok(()) => {
                println!(
                    "[remote] committed {} chars via fcitx5",
                    text.chars().count()
                );
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "[remote] fcitx5 CommitText failed: {error}; falling back to clipboard paste"
                );
            }
        }
    }

    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| format!("clipboard set: {e}"))?;
    println!(
        "[remote] fcitx5 提交失败，{} 字已复制到剪贴板（Wayland 下可手动粘贴）",
        text.chars().count()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn commit_text_via_fcitx(text: &str) -> Result<(), String> {
    use dbus::blocking::BlockingSender;
    let conn =
        dbus::blocking::Connection::new_session().map_err(|e| format!("dbus session: {e}"))?;
    let msg = dbus::Message::new_method_call(DBUS_DEST, DBUS_PATH, DBUS_IFACE, "CommitText")
        .map_err(|e| format!("build msg: {e}"))?
        .append1(text);
    conn.send_with_reply_and_block(msg, Duration::from_secs(3))
        .map_err(|e| format!("CommitText: {e}"))?;
    Ok(())
}

// ───────────────────────── fcitx5 提示区 ─────────────────────────

fn set_aux_down(text: &str) -> Result<(), String> {
    AUX_TOKEN.fetch_add(1, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    {
        use dbus::blocking::BlockingSender;
        let conn =
            dbus::blocking::Connection::new_session().map_err(|e| format!("dbus session: {e}"))?;
        let msg = dbus::Message::new_method_call(DBUS_DEST, DBUS_PATH, DBUS_IFACE, "SetAuxDown")
            .map_err(|e| format!("build msg: {e}"))?
            .append1(text);
        conn.send_with_reply_and_block(msg, Duration::from_secs(2))
            .map_err(|e| format!("SetAuxDown: {e}"))?;
    }
    Ok(())
}

fn clear_aux_down() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use dbus::blocking::BlockingSender;
        let conn =
            dbus::blocking::Connection::new_session().map_err(|e| format!("dbus session: {e}"))?;
        let msg = dbus::Message::new_method_call(DBUS_DEST, DBUS_PATH, DBUS_IFACE, "ClearAuxDown")
            .map_err(|e| format!("build msg: {e}"))?;
        conn.send_with_reply_and_block(msg, Duration::from_secs(2))
            .map_err(|e| format!("ClearAuxDown: {e}"))?;
    }
    Ok(())
}

fn flash_aux(text: &str) {
    let token = AUX_TOKEN.load(Ordering::Relaxed) + 1;
    let _ = set_aux_down(text);
    std::thread::sleep(Duration::from_secs(3));
    // 期间若有新的状态/提示写入，不再清除它。
    if AUX_TOKEN.load(Ordering::Relaxed) == token {
        let _ = clear_aux_down();
    }
}
