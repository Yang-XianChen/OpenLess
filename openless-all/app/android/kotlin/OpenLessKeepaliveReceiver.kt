package com.openless.app

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log

/**
 * 外部调度保活器：不依赖 App 进程存活的唤醒源。
 *
 * 三种触发：
 * 1. `BOOT_COMPLETED` —— 设备重启后自动拉起；
 * 2. `MY_PACKAGE_REPLACED` —— 应用升级后自动拉起；
 * 3. `com.openless.app.keepalive.CHECK` —— AlarmManager 周期唤醒（每 10 分钟一次）。
 *
 * 每次触发：若保活开关（通知 or 单像素）关闭则不强制拉活（只顺延下一次闹钟）；
 * 否则先确保 LAN 后端存活，再按需拉起 Overlay 前台服务。Receiver 自身在 App 进程内，
 * 进程被系统杀掉后，闹钟/开机广播会重新创建进程来执行本类 —— 这是「看门狗随进程
 * 一起消失」问题的外部兜底。
 */
class OpenLessKeepaliveReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        val enabled = OpenLessAndroidPreferences.notificationKeepalive(context) ||
            OpenLessAndroidPreferences.singlePixelKeepalive(context)
        if (enabled) {
            restoreBackend(context)
        }
        scheduleNext(context)
    }

    /**
     * 保活开关开启时恢复后端：
     * - LAN 服务未监听 → 先 nativeEnsureRemoteBackend 原地拉起（进程已在本次广播中恢复）；
     * - 通知保活 → 拉起前台服务（走 specialUse idle 类型）让常驻通知与遥测恢复；
     * - 单像素保活 → 补挂 1×1 悬浮窗。
     */
    private fun restoreBackend(context: Context) {
        val lanRunning = runCatching { OpenLessNative.nativeIsLanServerRunning() }
            .getOrDefault(false)
        if (!lanRunning) {
            runCatching { OpenLessNative.nativeEnsureRemoteBackend(context) }
                .onFailure { Log.w(TAG, "ensure remote backend failed", it) }
        }
        if (OpenLessAndroidPreferences.singlePixelKeepalive(context)) {
            startOverlayService(context, OpenLessOverlayService.ACTION_KEEPALIVE_SHOW)
        }
        if (OpenLessAndroidPreferences.notificationKeepalive(context)) {
            startOverlayService(context, null)
        }
    }

    private fun startOverlayService(context: Context, action: String?) {
        try {
            val intent = Intent(context, OpenLessOverlayService::class.java).apply {
                if (action != null) {
                    this.action = action
                }
            }
            if (OpenLessAndroidPreferences.notificationKeepalive(context)) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        } catch (error: Throwable) {
            Log.w(TAG, "start overlay service failed", error)
        }
    }

    companion object {
        const val ACTION_KEEPALIVE_CHECK = "com.openless.app.keepalive.CHECK"
        const val INTERVAL_MILLIS = 10L * 60L * 1000L
        private const val ALARM_REQUEST_CODE = 42002
        private const val TAG = "OpenLessKeepaliveReceiver"

        /** 安排下一次周期检查。使用非精确闹钟（免 SCHEDULE_EXACT_ALARM 权限）。 */
        fun scheduleNext(context: Context) {
            val alarmManager = context.getSystemService(Context.ALARM_SERVICE) as? AlarmManager
                ?: return
            val pendingIntent = buildPendingIntent(context)
            val triggerAt = System.currentTimeMillis() + INTERVAL_MILLIS
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    alarmManager.setAndAllowWhileIdle(
                        AlarmManager.RTC_WAKEUP,
                        triggerAt,
                        pendingIntent,
                    )
                } else {
                    alarmManager.set(AlarmManager.RTC_WAKEUP, triggerAt, pendingIntent)
                }
                Log.i(TAG, "next keepalive check scheduled in ${INTERVAL_MILLIS / 60000L}min")
            } catch (error: Throwable) {
                Log.w(TAG, "schedule keepalive check failed", error)
            }
        }

        /** 进程级自测用：安排一次 1 秒后的立即检查，作为 START_STICKY 的兜底唤醒。 */
        fun scheduleImmediateCheck(context: Context) {
            val alarmManager = context.getSystemService(Context.ALARM_SERVICE) as? AlarmManager
                ?: return
            val pendingIntent = buildPendingIntent(context)
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    alarmManager.setAndAllowWhileIdle(
                        AlarmManager.RTC_WAKEUP,
                        System.currentTimeMillis() + 1000L,
                        pendingIntent,
                    )
                } else {
                    alarmManager.set(
                        AlarmManager.RTC_WAKEUP,
                        System.currentTimeMillis() + 1000L,
                        pendingIntent,
                    )
                }
                Log.i(TAG, "immediate keepalive check armed (process-kill self-test)")
            } catch (error: Throwable) {
                Log.w(TAG, "schedule immediate keepalive check failed", error)
            }
        }

        /** 取消周期检查（关闭保活 / 卸载清理时用）。 */
        fun cancel(context: Context) {
            val alarmManager = context.getSystemService(Context.ALARM_SERVICE) as? AlarmManager
                ?: return
            alarmManager.cancel(buildPendingIntent(context))
        }

        private fun buildPendingIntent(context: Context): PendingIntent {
            val intent = Intent(context, OpenLessKeepaliveReceiver::class.java)
            return PendingIntent.getBroadcast(
                context,
                ALARM_REQUEST_CODE,
                intent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        }
    }
}