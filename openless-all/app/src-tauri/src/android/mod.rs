//! Android platform integration (JNI, overlay, accessibility, insert).

pub mod accessibility;
#[cfg(target_os = "android")]
pub mod insert;
pub mod insert_tiers;
pub mod jni;
pub mod lan_server;
pub mod native_bridge;
pub mod overlay;
pub mod shizuku;
#[cfg(target_os = "android")]
pub mod updater;
pub mod updater_logic;
pub use crate::types::android_types as types;

pub use accessibility::{
    get_android_accessibility_status, is_accessibility_enabled, paste_via_accessibility,
    paste_via_accessibility_with_result, request_android_accessibility_permission,
    AndroidAccessibilityPermissionResult,
};
#[cfg(target_os = "android")]
pub use insert::android_insert_with_strategy;
pub use native_bridge::{
    hide_overlay, is_overlay_visible, notify_capsule_state, refresh_overlay_if_visible,
    refresh_overlay_layout, register_android_coordinator, replace_overlay, show_overlay,
};
pub use overlay::{
    apply_single_pixel_keepalive, get_android_overlay_status, hide_android_overlay,
    refresh_android_overlay_if_visible, refresh_android_overlay_layout, replace_android_overlay,
    request_android_overlay_permission, show_android_overlay, AndroidOverlayPermissionResult,
};
pub use shizuku::{
    get_android_shizuku_status, open_shizuku_app, paste_via_shizuku_with_result,
    recover_android_accessibility, request_android_shizuku_permission, AndroidShizukuOpenResult,
    AndroidShizukuPermissionResult,
};

/// 远程听写保活诊断状态：由 Kotlin 服务汇总原生状态后返回 JSON。
pub fn get_android_keepalive_status() -> serde_json::Value {
    #[cfg(target_os = "android")]
    {
        use jni::objects::JValue;

        let result = crate::android::jni::android::with_android_env(|env, context| {
            let class = crate::android::jni::android::load_context_class(
                env,
                context,
                "com.openless.app.OpenLessOverlayService",
            )?;
            let value = env
                .call_static_method(
                    &class,
                    "getKeepaliveStatusJson",
                    "(Landroid/content/Context;)Ljava/lang/String;",
                    &[JValue::Object(context)],
                )
                .and_then(|value| value.l())
                .map_err(|error| format!("call getKeepaliveStatusJson: {error}"))?;
            if value.is_null() {
                return Err("getKeepaliveStatusJson returned null".to_string());
            }
            let text = env
                .get_string(&jni::objects::JString::from(value))
                .map_err(|error| format!("read keepalive status json: {error}"))?
                .to_string_lossy()
                .into_owned();
            serde_json::from_str(&text)
                .map_err(|error| format!("parse keepalive status json: {error}"))
        });
        result.unwrap_or_else(|error| {
            serde_json::json!({
                "lanServerRunning": crate::android::lan_server::lan_server_is_running(),
                "foregroundServiceRunning": false,
                "notificationKeepaliveEnabled": false,
                "notificationPermissionGranted": false,
                "overlayPermissionGranted": false,
                "batteryOptimizationRestricted": true,
                "lastError": error,
                "lastCheckAt": crate::mobile_runtime::keepalive_last_check_at(),
                "lastStatus": crate::mobile_runtime::keepalive_last_status(),
                "autoRecoverySupported": true,
            })
        })
    }

    #[cfg(not(target_os = "android"))]
    {
        serde_json::json!({
            "lanServerRunning": false,
            "foregroundServiceRunning": false,
            "notificationKeepaliveEnabled": false,
            "notificationPermissionGranted": false,
            "overlayPermissionGranted": false,
            "batteryOptimizationRestricted": true,
            "lastError": null,
            "lastCheckAt": null,
            "lastStatus": null,
            "autoRecoverySupported": false,
        })
    }
}

fn call_android_settings_method(method: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        use jni::objects::JValue;
        crate::android::jni::android::with_android_env(|env, context| {
            let class = crate::android::jni::android::load_context_class(
                env,
                context,
                "com.openless.app.OpenLessOverlayService",
            )?;
            env.call_static_method(
                &class,
                method,
                "(Landroid/content/Context;)V",
                &[JValue::Object(context)],
            )
            .map_err(|error| format!("call {method}: {error}"))?;
            Ok(())
        })
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = method;
        Err("Android settings are only available on Android".to_string())
    }
}

pub fn open_android_notification_settings() -> Result<(), String> {
    call_android_settings_method("openNotificationSettings")
}

pub fn open_android_battery_settings() -> Result<(), String> {
    call_android_settings_method("openBatteryOptimizationSettings")
}
