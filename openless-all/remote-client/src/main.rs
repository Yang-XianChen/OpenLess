//! OpenLess 局域网远程听写 —— 极简无界面电脑端。
//!
//! 用法：
//! ```text
//! openless-remote-client --address 192.168.1.20:45678 [--hotkey "Ctrl+Shift+Space"]
//! ```
//!
//! 按住热键：向手机 OpenLess 发送 `start`，手机开始录音/ASR/润色；
//! 松开热键：发送 `stop`，手机处理完成后回传 `final_text`，本程序模拟 Ctrl+V
//! （macOS 为 Cmd+V）把文本插入当前光标位置。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use clap::Parser;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde_json::json;
use tungstenite::{connect, Message};

type ClientSocket =
    tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

static SESSION: OnceLock<Mutex<Option<ClientSocket>>> = OnceLock::new();

#[derive(Parser, Debug)]
#[command(name = "openless-remote-client", about = "Headless OpenLess Android LAN dictation client")]
struct Args {
    /// 手机地址，例如 192.168.1.20:45678
    #[arg(long)]
    address: String,
    /// 触发热键，例如 Ctrl+Shift+Space
    #[arg(long, default_value = "Ctrl+Shift+Space")]
    hotkey: String,
    /// 切换模式：按一下开始，再按一下结束（默认按住说话模式）
    #[arg(long)]
    toggle: bool,
    /// 使用 fcitx5 DBus 热键通道（Wayland 下捕获修饰键需要）
    #[arg(long)]
    fcitx: bool,
    /// 可选配置文件（未实现，仅保留占位）
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    let _ = SESSION.set(Mutex::new(None));

    if args.fcitx {
        run_fcitx_toggle(&args.address);
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
    println!(
        "[remote] listening {} -> ws://{} ({})",
        args.hotkey,
        args.address,
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
                        println!("[remote] stopping…");
                        if let Err(error) = stop_session(&args.address) {
                            eprintln!("[remote] stop failed: {error}");
                        }
                        recording = false;
                    } else {
                        println!("[remote] starting…");
                        if let Err(error) = start_session(&args.address) {
                            eprintln!("[remote] start failed: {error}");
                        } else {
                            recording = true;
                        }
                    }
                } else if let Err(error) = start_session(&args.address) {
                    eprintln!("[remote] start failed: {error}");
                }
            }
            HotKeyState::Released => {
                if !args.toggle {
                    println!("[remote] hotkey released");
                    if let Err(error) = stop_session(&args.address) {
                        eprintln!("[remote] stop failed: {error}");
                    }
                }
            }
        }
    }
}

const DBUS_DEST: &str = "org.fcitx.Fcitx5";
const DBUS_PATH: &str = "/openless";
const DBUS_IFACE: &str = "org.fcitx.Fcitx.OpenLess1";
const KEYSYM_ALT_R: u32 = 0xffea;

/// Wayland 方案：通过 fcitx5 OpenLess 插件监听右 Alt 键事件。
/// 右 Alt 按下 → 切换 开始/停止；松手不处理（toggle 模式）。
fn run_fcitx_toggle(address: &str) -> ! {
    use dbus::blocking::SyncConnection;
    use std::sync::mpsc;
    use std::time::Duration;

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
        if member == "DictationKeyEvent" && args.2 {
            let _ = tx.send(true);
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

    println!("[remote] fcitx5 Right Alt toggle -> ws://{address}");
    let mut recording = false;
    loop {
        let _ = conn.process(Duration::from_millis(200));
        while let Ok(true) = rx.try_recv() {
            if recording {
                println!("[remote] stopping…");
                if let Err(error) = stop_session(address) {
                    eprintln!("[remote] stop failed: {error}");
                }
                recording = false;
            } else {
                println!("[remote] starting…");
                if let Err(error) = start_session(address) {
                    eprintln!("[remote] start failed: {error}");
                } else {
                    recording = true;
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
    match conn.send_with_reply_and_block(msg, std::time::Duration::from_secs(1)) {
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
    conn.send_with_reply_and_block(msg, std::time::Duration::from_secs(3))
        .map_err(|e| format!("SetHotkeyRaw: {e}"))?;
    Ok(())
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
        let frame = socket
            .read()
            .map_err(|e| format!("read ack: {e}"))?;
        match frame {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text.as_str())
                    .map_err(|e| format!("bad ack json: {e}"))?;
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

fn insert_text(text: &str) -> Result<(), String> {
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
