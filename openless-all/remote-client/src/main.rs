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
//! 正在录音 → 正在转录 → 完成清除；连接失败会显示失败提示并自动重试一次。

use std::path::PathBuf;
use std::net::IpAddr;
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clap::Parser;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde_json::json;
use tungstenite::{connect, Message};

type ClientSocket =
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

static SESSION: OnceLock<Mutex<Option<ClientSocket>>> = OnceLock::new();
static DISCOVERED: OnceLock<Mutex<Option<String>>> = OnceLock::new();

const DEFAULT_PORT: u16 = 45678;
const AUX_RECORDING: &str = "🎤 正在录音…";
const AUX_TRANSCRIBING: &str = "⏳ 正在转录…";
const AUX_SEARCHING: &str = "🔍 正在搜索手机…";
const AUX_RETRY: &str = "连接失败，正在重试…";
const AUX_FAILED: &str = "❌ 连接失败";

#[derive(Parser, Debug)]
#[command(name = "openless-remote-client", about = "Headless OpenLess Android LAN dictation client")]
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

    if args.fcitx {
        run_fcitx(args.address.clone(), args.auto_discover, args.toggle);
    }

    let hotkey = match parse_hotkey(&args.hotkey) {
        Ok(hotkey) => hotkey,
        Err(error) => {
            eprintln!("[remote] invalid hotkey {:?}: {error}", args.hotkey);
            std::process::exit(1);
        }
    };

    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("[remote] failed to init global hotkey: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = manager.register(hotkey) {
        eprintln!("[remote] failed to register hotkey {}: {error}", args.hotkey);
        std::process::exit(1);
    }

    let mut recording = false;
    let mut session_address: Option<String> = None;
    println!(
        "[remote] listening {} ({})",
        args.hotkey,
        if args.toggle { "toggle" } else { "hold to talk" }
    );

    let receiver = GlobalHotKeyEvent::receiver();
    loop {
        let event = match receiver.recv() {
            Ok(event) => event,
            Err(error) => {
                eprintln!("[remote] hotkey event channel closed: {error}");
                break;
            }
        };
        if event.id() != hotkey.id() {
            continue;
        }
        match event.state() {
            HotKeyState::Pressed => {
                println!("[remote] hotkey pressed");
                if args.toggle {
                    if recording {
                        recording = false;
                        if let Some(address) = session_address.take() {
                            stop_flow(&address);
                        }
                    } else if !recording {
                        match start_flow(args.address.as_deref(), args.auto_discover) {
                            Ok(address) => {
                                session_address = Some(address);
                                recording = true;
                            }
                            Err(error) => eprintln!("[remote] start failed: {error}"),
                        }
                    }
                } else if !recording {
                    match start_flow(args.address.as_deref(), args.auto_discover) {
                        Ok(address) => {
                            session_address = Some(address);
                            recording = true;
                        }
                        Err(error) => eprintln!("[remote] start failed: {error}"),
                    }
                }
            }
            HotKeyState::Released => {
                if !args.toggle && recording {
                    recording = false;
                    if let Some(address) = session_address.take() {
                        stop_flow(&address);
                    }
                }
            }
        }
    }
}

// ───────────────────────── fcitx5 通道 ─────────────────────────

const DBUS_DEST: &str = "org.fcitx.Fcitx5";
const DBUS_PATH: &str = "/openless";
const DBUS_IFACE: &str = "org.fcitx.Fcitx.OpenLess1";
const KEYSYM_ALT_R: u32 = 0xffea;

/// Wayland 方案：通过 fcitx5 OpenLess 插件监听右 Alt 键事件，
/// 并在输入法提示区显示录音/转录/失败状态。
fn run_fcitx(address: Option<String>, auto_discover: bool, toggle: bool) -> ! {
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

    println!("[remote] fcitx5 Right Alt active ({})", if toggle { "toggle" } else { "hold to talk" });
    let mut recording = false;
    let mut session_address: Option<String> = None;
    loop {
        let _ = conn.process(Duration::from_millis(200));
        while let Ok(is_press) = rx.try_recv() {
            if is_press {
                if toggle {
                    if recording {
                        recording = false;
                        if let Some(address) = session_address.take() {
                            stop_flow(&address);
                        }
                    } else if !recording {
                        match start_flow(address.as_deref(), auto_discover) {
                            Ok(found) => {
                                session_address = Some(found);
                                recording = true;
                            }
                            Err(error) => eprintln!("[remote] start failed: {error}"),
                        }
                    }
                } else if !recording {
                    match start_flow(address.as_deref(), auto_discover) {
                        Ok(found) => {
                            session_address = Some(found);
                            recording = true;
                        }
                        Err(error) => eprintln!("[remote] start failed: {error}"),
                    }
                }
            } else if !toggle && recording {
                recording = false;
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

fn start_flow(address: Option<&str>, auto_discover: bool) -> Result<String, String> {
    let _ = set_aux_down(AUX_RECORDING);
    let mut last_error = "未连接".to_string();
    for attempt in 0..2 {
        let resolved = match resolve_address(address, auto_discover) {
            Ok(resolved) => resolved,
            Err(error) => {
                last_error = error;
                if attempt == 0 {
                    let _ = set_aux_down(AUX_SEARCHING);
                }
                continue;
            }
        };
        println!("[remote] connecting {resolved} (attempt {})", attempt + 1);
        match start_session(&resolved) {
            Ok(()) => {
                if let Some(cache) = DISCOVERED.get() {
                    *cache.lock().unwrap() = Some(resolved.clone());
                }
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
    flash_aux(AUX_FAILED);
    Err(last_error)
}

fn stop_flow(address: &str) {
    let _ = set_aux_down(AUX_TRANSCRIBING);
    match stop_session(address) {
        Ok(()) => {
            let _ = clear_aux_down();
        }
        Err(error) => {
            let message = format!("❌ 转录失败：{error}");
            flash_aux(&message);
        }
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
        match discover_phone(DEFAULT_PORT) {
            Some(found) => {
                if let Some(cache) = DISCOVERED.get() {
                    *cache.lock().unwrap() = Some(found.clone());
                }
                Ok(found)
            }
            None => Err("未发现手机设备".to_string()),
        }
    } else {
        Err("未配置手机地址（--address 或 --auto-discover）".to_string())
    }
}

/// 扫描本机局域网 /24 网段里监听指定端口并能回复 OpenLess ping 的设备。
fn discover_phone(port: u16) -> Option<String> {
    let mut prefixes = Vec::new();
    if let Ok(interfaces) = local_ip_address::list_afinet_netifas() {
        for (_name, ip) in interfaces {
            if let IpAddr::V4(v4) = ip {
                if v4.is_loopback() || v4.is_link_local() {
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
    if prefixes.is_empty() {
        return None;
    }

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::scope(|scope| {
        for prefix in prefixes {
            for host in 1..=254 {
                let tx = tx.clone();
                let prefix = prefix.clone();
                let url = format!("ws://{prefix}.{host}:{port}");
                scope.spawn(move || {
                    if let Ok((mut socket, _)) = connect(&url) {
                        if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = socket.get_ref() {
                            let _ = tcp.set_read_timeout(Some(Duration::from_millis(1200)));
                        }
                        let _ = socket.send(Message::Text(json!({"type":"ping"}).to_string().into()));
                        if let Ok(Message::Text(text)) = socket.read() {
                            if text.contains("\"pong\"") {
                                let _ = tx.send(format!("{prefix}.{host}:{port}"));
                            }
                        }
                    }
                });
            }
        }
        rx.recv_timeout(Duration::from_secs(10)).ok()
    })
}

fn start_session(address: &str) -> Result<(), String> {
    let url = format!("ws://{address}");
    let (mut socket, _response) = connect(&url).map_err(|e| format!("connect {url}: {e}"))?;
    send_json(
        &mut socket,
        json!({ "type": "start", "translation": false }),
    )?;
    wait_for_type(&mut socket, &["started"])?;
    *SESSION
        .get()
        .expect("SESSION initialized in main")
        .lock()
        .unwrap() = Some(socket);
    Ok(())
}

fn stop_session(address: &str) -> Result<(), String> {
    let mut guard = SESSION
        .get()
        .expect("SESSION initialized in main")
        .lock()
        .unwrap();
    let Some(mut socket) = guard.take() else {
        return Err("no active session (press/release out of sync?)".to_string());
    };
    send_json(
        &mut socket,
        json!({ "type": "stop", "translation": false }),
    )?;

    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for phone result".to_string());
        }
        let frame = socket
            .read()
            .map_err(|e| format!("read from {address}: {e}"))?;
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad json from phone: {e}"))?;
                match value["type"].as_str() {
                    Some("result") => {
                        let text = value["text"].as_str().unwrap_or_default().to_string();
                        if text.is_empty() {
                            return Err("phone returned empty text".to_string());
                        }
                        insert_text(&text)?;
                        return Ok(());
                    }
                    Some("error") => {
                        return Err(value["message"]
                            .as_str()
                            .unwrap_or("phone error")
                            .to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => return Err("phone closed connection before result".to_string()),
            _ => {}
        }
    }
}

fn wait_for_type(socket: &mut ClientSocket, expected: &[&str]) -> Result<(), String> {
    loop {
        let frame = socket.read().map_err(|e| format!("read ack: {e}"))?;
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value =
                    serde_json::from_str(text.as_str()).map_err(|e| format!("bad ack json: {e}"))?;
                match value["type"].as_str() {
                    Some(kind) if expected.contains(&kind) => return Ok(()),
                    Some("error") => {
                        return Err(value["message"]
                            .as_str()
                            .unwrap_or("phone error")
                            .to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => return Err("phone closed connection during ack".to_string()),
            _ => {}
        }
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
                println!("[remote] committed {} chars via fcitx5", text.chars().count());
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

    use enigo::{Direction, Enigo, Keyboard, Settings};
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| format!("enigo: {e}"))?;

    #[cfg(target_os = "macos")]
    let (modifiers, primary) = (vec![enigo::Key::Meta], enigo::Key::Unicode('v'));
    #[cfg(not(target_os = "macos"))]
    let (modifiers, primary) = (vec![enigo::Key::Control], enigo::Key::Unicode('v'));

    let mut pressed = 0usize;
    let mut first_error: Option<String> = None;
    for modifier in &modifiers {
        if let Err(error) = enigo.key(*modifier, Direction::Press) {
            first_error = Some(error.to_string());
            break;
        }
        pressed += 1;
    }
    if first_error.is_none() {
        if let Err(error) = enigo.key(primary, Direction::Click) {
            first_error = Some(error.to_string());
        }
    }
    for modifier in modifiers[..pressed].iter().rev() {
        if let Err(error) = enigo.key(*modifier, Direction::Release) {
            if first_error.is_none() {
                first_error = Some(error.to_string());
            }
        }
    }
    match first_error {
        Some(error) => Err(format!("paste simulation failed: {error}")),
        None => {
            println!("[remote] inserted {} chars", text.chars().count());
            Ok(())
        }
    }
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
    let _ = set_aux_down(text);
    std::thread::sleep(Duration::from_secs(3));
    let _ = clear_aux_down();
}

// ───────────────────────── 热键解析 ─────────────────────────

fn parse_hotkey(raw: &str) -> Result<HotKey, String> {
    let mut modifiers = Modifiers::empty();
    let mut code: Option<Code> = None;
    for part in raw.split('+').map(str::trim) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => modifiers |= Modifiers::CONTROL,
            "alt" | "option" | "opt" => modifiers |= Modifiers::ALT,
            "shift" => modifiers |= Modifiers::SHIFT,
            "super" | "cmd" | "command" | "meta" | "win" => modifiers |= Modifiers::SUPER,
            "rightalt" | "altright" | "ralt" | "右alt" => code = Some(Code::AltRight),
            "leftalt" | "altleft" | "lalt" | "左alt" => code = Some(Code::AltLeft),
            "space" => code = Some(Code::Space),
            "enter" | "return" => code = Some(Code::Enter),
            "tab" => code = Some(Code::Tab),
            "esc" | "escape" => code = Some(Code::Escape),
            "f1" => code = Some(Code::F1),
            "f2" => code = Some(Code::F2),
            "f3" => code = Some(Code::F3),
            "f4" => code = Some(Code::F4),
            "f5" => code = Some(Code::F5),
            "f6" => code = Some(Code::F6),
            "f7" => code = Some(Code::F7),
            "f8" => code = Some(Code::F8),
            "f9" => code = Some(Code::F9),
            "f10" => code = Some(Code::F10),
            "f11" => code = Some(Code::F11),
            "f12" => code = Some(Code::F12),
            single if single.chars().count() == 1 => {
                code = Some(char_to_code(single.chars().next().unwrap())?);
            }
            other => return Err(format!("unsupported key: {other}")),
        }
    }
    let code = code.ok_or_else(|| "hotkey needs a main key".to_string())?;
    Ok(HotKey::new(Some(modifiers), code))
}

fn char_to_code(ch: char) -> Result<Code, String> {
    let upper = ch.to_ascii_uppercase();
    let code = match upper {
        'A' => Code::KeyA,
        'B' => Code::KeyB,
        'C' => Code::KeyC,
        'D' => Code::KeyD,
        'E' => Code::KeyE,
        'F' => Code::KeyF,
        'G' => Code::KeyG,
        'H' => Code::KeyH,
        'I' => Code::KeyI,
        'J' => Code::KeyJ,
        'K' => Code::KeyK,
        'L' => Code::KeyL,
        'M' => Code::KeyM,
        'N' => Code::KeyN,
        'O' => Code::KeyO,
        'P' => Code::KeyP,
        'Q' => Code::KeyQ,
        'R' => Code::KeyR,
        'S' => Code::KeyS,
        'T' => Code::KeyT,
        'U' => Code::KeyU,
        'V' => Code::KeyV,
        'W' => Code::KeyW,
        'X' => Code::KeyX,
        'Y' => Code::KeyY,
        'Z' => Code::KeyZ,
        '0' => Code::Digit0,
        '1' => Code::Digit1,
        '2' => Code::Digit2,
        '3' => Code::Digit3,
        '4' => Code::Digit4,
        '5' => Code::Digit5,
        '6' => Code::Digit6,
        '7' => Code::Digit7,
        '8' => Code::Digit8,
        '9' => Code::Digit9,
        ';' | ':' => Code::Semicolon,
        ',' | '<' => Code::Comma,
        '.' | '>' => Code::Period,
        '/' | '?' => Code::Slash,
        '\\' | '|' => Code::Backslash,
        '[' | '{' => Code::BracketLeft,
        ']' | '}' => Code::BracketRight,
        '\'' | '"' => Code::Quote,
        '`' | '~' => Code::Backquote,
        '-' | '_' => Code::Minus,
        '=' | '+' => Code::Equal,
        ' ' => Code::Space,
        other => return Err(format!("unsupported main key: {other}")),
    };
    Ok(code)
}
