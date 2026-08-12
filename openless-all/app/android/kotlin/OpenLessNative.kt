package com.openless.app

import android.content.Context

/**
 * JNI bridge from Kotlin overlay / lifecycle code into Rust Coordinator.
 */
object OpenLessNative {
    init {
        try {
            System.loadLibrary("openless_lib")
        } catch (error: UnsatisfiedLinkError) {
            android.util.Log.e("OpenLessNative", "failed to load openless_lib", error)
        }
    }

    @JvmStatic external fun nativeStartDictation()

    @JvmStatic external fun nativeStartDictationWithTranslation(translation: Boolean)

    @JvmStatic external fun nativeStopDictation()

    @JvmStatic external fun nativeStopDictationWithTranslation(translation: Boolean)

    @JvmStatic external fun nativeCancelDictation()

    @JvmStatic external fun nativeSwitchStylePack()

    @JvmStatic external fun nativeOpenQaFromOverlay()

    @JvmStatic external fun nativeFinalizeQaFromOverlay()

    @JvmStatic external fun nativeGetOverlayTriggerMode(): String

    @JvmStatic external fun nativeCanDrawOverlays(context: android.content.Context): Boolean

    @JvmStatic external fun nativeShowOverlay(context: android.content.Context)

    @JvmStatic external fun nativeHideOverlay(context: android.content.Context)

    @JvmStatic external fun nativeIsOverlayVisible(): Boolean

    @JvmStatic external fun nativeNotifyOverlayPermissionChanged(context: android.content.Context)

    @JvmStatic external fun nativeNotifyOverlayDestroyed()

    @JvmStatic external fun nativeEnsureRemoteBackend(context: Context): Boolean

    @JvmStatic external fun nativeIsLanServerRunning(): Boolean

    @JvmStatic external fun nativeGetLanServerLastError(): String?

    @JvmStatic external fun nativeGetKeepaliveLastCheckAt(): String?

    @JvmStatic external fun nativeGetKeepaliveLastStatus(): String?
}
