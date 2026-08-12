//! Minimal Tauri mobile runtime — single main window, no tray/hotkey/updater.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[cfg(target_os = "android")]
use jni::objects::JObject;
#[cfg(target_os = "android")]
use jni::JNIEnv;

use tauri::{AppHandle, Manager, RunEvent};

use crate::coordinator::Coordinator;

#[cfg(target_os = "android")]
static MOBILE_COORDINATOR: OnceLock<Arc<Coordinator>> = OnceLock::new();
#[cfg(target_os = "android")]
static BACKEND_READY: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static RUNTIME_SET: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static ANDROID_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
#[cfg(target_os = "android")]
static LAST_KEEPALIVE_CHECK_AT: Mutex<Option<String>> = Mutex::new(None);
#[cfg(target_os = "android")]
static LAST_KEEPALIVE_STATUS: Mutex<Option<String>> = Mutex::new(None);

pub fn run() {
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init());
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let builder = builder.plugin(tauri_plugin_fs::init());

    // Coordinator is created inside setup (after Android storage roots are ready).
    // Managing state in setup is supported by Tauri 2 and avoids constructing
    // PreferencesStore against /data/local/tmp before JNI Context exists.
    builder
        .setup(|app| {
            #[cfg(target_os = "android")]
            {
                if let Err(error) = crate::persistence::init_android_storage_roots() {
                    eprintln!("[android-storage] ERROR init failed: {error:#}");
                }
            }

            crate::init_file_logger();
            log::info!("=== OpenLess mobile 启动 ===");
            initialize_android_ndk_context_for_audio();

            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
            }
            if let Some(qa) = app.get_webview_window("qa") {
                let _ = qa.hide();
            }

            #[cfg(target_os = "android")]
            let coordinator = {
                if let Some(existing) = MOBILE_COORDINATOR.get() {
                    existing.clone()
                } else {
                    let created = Arc::new(Coordinator::new());
                    let _ = MOBILE_COORDINATOR.set(created.clone());
                    created
                }
            };
            #[cfg(not(target_os = "android"))]
            let coordinator = Arc::new(Coordinator::new());

            app.manage(coordinator.clone());
            coordinator.bind_app(app.handle().clone());
            #[cfg(target_os = "android")]
            {
                crate::android::register_android_coordinator(coordinator.clone());
                let already_started = BACKEND_READY.swap(true, AtomicOrdering::SeqCst);
                if !already_started {
                    crate::android::lan_server::ensure_started(coordinator.clone());
                    if coordinator.android_notification_keepalive_enabled() {
                        if let Err(error) =
                            crate::android::native_bridge::promote_remote_recording()
                        {
                            log::warn!(
                                "[android] remote recording foreground service start failed: {error}"
                            );
                        }
                    }
                    coordinator.apply_android_overlay_on_startup();
                } else {
                    crate::android::lan_server::ensure_started(coordinator.clone());
                    if coordinator.android_notification_keepalive_enabled() {
                        if let Err(error) =
                            crate::android::native_bridge::promote_remote_recording()
                        {
                            log::warn!(
                                "[android] remote recording foreground service start failed: {error}"
                            );
                        }
                    }
                }
                start_keepalive_watchdog(coordinator.clone());
            }
            Ok(())
        })
        .invoke_handler(crate::app_invoke_handler_mobile!())
        .build(tauri::generate_context!())
        .expect("error while building tauri mobile application")
        .run(|app, event| match event {
            RunEvent::Exit => {
                if let Some(coordinator) = app.try_state::<Arc<Coordinator>>() {
                    coordinator.stop_hotkey_listener();
                }
            }
            _ => {}
        });
}

/// 供 Kotlin 前台服务在进程被系统重建时调用，重新拉起 Rust 后端与 LAN 服务。
#[cfg(target_os = "android")]
pub(crate) fn ensure_android_backend_from_jni(
    env: &mut JNIEnv,
    context: &JObject,
) -> Result<(), String> {
    if BACKEND_READY.load(AtomicOrdering::Relaxed) {
        if let Some(coordinator) = MOBILE_COORDINATOR.get() {
            crate::android::lan_server::ensure_started(coordinator.clone());
        }
        let running = crate::android::lan_server::lan_server_is_running();
        record_keepalive_check(
            running,
            if running {
                "backend ready, lan server listening"
            } else {
                "backend ready, lan server not listening"
            },
        );
        return Ok(());
    }

    initialize_ndk_context_from_jni(env, context)?;
    if let Err(error) = crate::persistence::init_android_storage_roots() {
        log::warn!("[android-storage] ERROR init failed: {error:#}");
    }
    crate::init_file_logger();

    let runtime = ANDROID_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build Android keepalive tokio runtime")
    });
    if !RUNTIME_SET.swap(true, AtomicOrdering::SeqCst) {
        tauri::async_runtime::set(runtime.handle().clone());
    }

    let coordinator = match MOBILE_COORDINATOR.get() {
        Some(existing) => existing.clone(),
        None => {
            let created = Arc::new(Coordinator::new());
            let _ = MOBILE_COORDINATOR.set(created.clone());
            created
        }
    };
    crate::android::register_android_coordinator(coordinator.clone());
    crate::android::lan_server::ensure_started(coordinator.clone());
    if coordinator.android_notification_keepalive_enabled() {
        if let Err(error) = crate::android::native_bridge::promote_remote_recording() {
            log::warn!("[android] remote recording foreground service start failed: {error}");
        }
    }
    coordinator.apply_android_overlay_on_startup();
    BACKEND_READY.store(true, AtomicOrdering::SeqCst);
    record_keepalive_check(true, "backend initialized from service");
    start_keepalive_watchdog(coordinator);
    Ok(())
}

#[cfg(target_os = "android")]
fn initialize_ndk_context_from_jni(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    let vm = env
        .get_java_vm()
        .map_err(|error| format!("get Android JVM: {error}"))?;
    let vm_ptr = vm.get_java_vm_pointer();
    let context_ptr = context.as_raw() as *mut std::ffi::c_void;
    let result = std::panic::catch_unwind(|| unsafe {
        ndk_context::initialize_android_context(vm_ptr.cast(), context_ptr);
    });
    if result.is_err() {
        log::warn!("[android] ndk-context already initialized or rejected initialization");
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn start_keepalive_watchdog(coordinator: Arc<Coordinator>) {
    if WATCHDOG_STARTED.swap(true, AtomicOrdering::SeqCst) {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(10));
        let running = crate::android::lan_server::lan_server_is_running();
        if running {
            record_keepalive_check(true, "lan server listening");
            continue;
        }
        let last_error = crate::android::lan_server::lan_server_last_error();
        log::warn!("[keepalive] LAN server not running; last_error={last_error:?}; retrying");
        record_keepalive_check(
            false,
            last_error.as_deref().unwrap_or("lan server not running"),
        );
        crate::android::lan_server::request_lan_server_restart(coordinator.clone());
    });
}

#[cfg(target_os = "android")]
fn record_keepalive_check(ok: bool, detail: &str) {
    let now = chrono::Local::now().to_rfc3339();
    *LAST_KEEPALIVE_CHECK_AT.lock().unwrap() = Some(now);
    *LAST_KEEPALIVE_STATUS.lock().unwrap() = Some(if ok {
        format!("ok: {detail}")
    } else {
        format!("failed: {detail}")
    });
}

#[cfg(target_os = "android")]
pub(crate) fn keepalive_last_check_at() -> Option<String> {
    LAST_KEEPALIVE_CHECK_AT.lock().unwrap().clone()
}

#[cfg(target_os = "android")]
pub(crate) fn keepalive_last_status() -> Option<String> {
    LAST_KEEPALIVE_STATUS.lock().unwrap().clone()
}

#[cfg(target_os = "android")]
pub(crate) fn request_android_lan_server_restart() -> Result<(), String> {
    let coordinator = MOBILE_COORDINATOR
        .get()
        .ok_or_else(|| "Android backend not initialized".to_string())?
        .clone();
    crate::android::lan_server::request_lan_server_restart(coordinator);
    Ok(())
}

#[cfg(target_os = "android")]
pub(crate) fn run_android_keepalive_self_test() -> Result<serde_json::Value, String> {
    crate::android::lan_server::simulate_lan_server_loss();
    let mut auto_recovered = false;
    for _ in 0..3 {
        request_android_lan_server_restart()?;
        for _ in 0..50 {
            if crate::android::lan_server::lan_server_is_running() {
                auto_recovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if auto_recovered {
            break;
        }
    }
    Ok(serde_json::json!({
        "simulatedFailure": true,
        "autoRecovered": auto_recovered,
    }))
}

#[allow(dead_code)]
pub(crate) fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg(target_os = "android")]
fn initialize_android_ndk_context_for_audio() {
    static INIT: std::sync::Once = std::sync::Once::new();

    INIT.call_once(|| {
        let Some(context) = tao::platform::android::prelude::main_android_context() else {
            log::warn!("[android] tao Android context unavailable; audio backend may fail");
            return;
        };

        let result = std::panic::catch_unwind(|| unsafe {
            ndk_context::initialize_android_context(context.java_vm, context.context_jobject);
        });

        if result.is_ok() {
            log::info!("[android] initialized ndk-context for audio backend");
        } else {
            log::warn!("[android] ndk-context was already initialized or rejected initialization");
        }
    });
}

#[cfg(not(target_os = "android"))]
fn initialize_android_ndk_context_for_audio() {}
