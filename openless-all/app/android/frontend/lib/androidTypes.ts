/** Android-specific preference and status types (mirrors Rust IPC payloads). */

export type AndroidInsertStrategy = 'accessibility' | 'clipboard';
export type AndroidOverlayTrigger = 'background' | 'keyboard' | 'always' | 'off';
export type AndroidOverlayActivationMode = 'tap' | 'long_press';
export type AndroidOverlayLeftSwipeAction = 'translation' | 'style_pack';
export type AndroidOverlayCancelSwipeDirection = 'up' | 'down';

export interface AndroidOverlayStatus {
  permission: 'granted' | 'notGranted' | 'notAndroid';
  overlayVisible: boolean;
  message: string;
}

export interface AndroidKeepaliveStatus {
  lanServerRunning: boolean;
  foregroundServiceRunning: boolean;
  notificationKeepaliveEnabled: boolean;
  notificationPermissionGranted: boolean;
  overlayPermissionGranted: boolean;
  batteryOptimizationRestricted: boolean;
  lastError?: string | null;
  lastCheckAt?: string | null;
  lastStatus?: string | null;
  autoRecoverySupported: boolean;
  /** 持久化的进程诊断：上次前台服务启动时刻 / 上次进程死亡时刻 / 重启计数。 */
  lastServiceStartAt?: string | null;
  lastProcessDeathAt?: string | null;
  restartCount?: number;
  /** 进程级自测结果：自杀式重启后 LAN 服务是否真的恢复。 */
  selfTestRecovered?: boolean;
  selfTestRecoveredAt?: string | null;
}

export interface AndroidAccessibilityStatus {
  state: 'enabled' | 'notEnabled' | 'notAndroid';
  enabled: boolean;
  operational?: boolean;
  message?: string;
  messageKey: string;
}

export type AndroidShizukuState =
  | 'notInstalled'
  | 'notRunning'
  | 'notAuthorized'
  | 'authorized'
  | 'binderDead'
  | 'notAndroid';

export interface AndroidAccessibilityDiagnosis {
  registered: boolean;
  operational: boolean;
  message?: string;
  messageKey: string;
}

export interface AndroidShizukuStatus {
  state: AndroidShizukuState;
  message?: string;
  messageKey: string;
  accessibility: AndroidAccessibilityDiagnosis;
  lastPermissionMessageKey?: string | null;
}

export type AndroidAccessibilityRecoveryOutcome =
  | 'success'
  | 'writeRejected'
  | 'serviceNotBound'
  | 'shizukuUnavailable'
  | 'userNotConfirmed'
  | 'shellFailed';

export interface AndroidAccessibilityRecoveryResult {
  outcome: AndroidAccessibilityRecoveryOutcome;
  message?: string;
  messageKey: string;
}

export interface AndroidShizukuActionResult {
  launched: boolean;
  message?: string;
  messageKey: string;
}

export type AndroidPreferenceKey =
  | 'androidInsertStrategy'
  | 'androidOverlayTrigger'
  | 'androidOverlayActivationMode'
  | 'androidOverlayLeftSwipeAction'
  | 'androidOverlayCancelSwipeDirection'
  | 'androidOverlaySizeDp'
  | 'androidSinglePixelKeepalive'
  | 'androidNotificationKeepalive';

export function normalizeAndroidOverlayTrigger(
  trigger: AndroidOverlayTrigger,
): AndroidOverlayTrigger {
  return trigger === 'keyboard' ? 'background' : trigger;
}

export function clampAndroidOverlaySize(size: number): number {
  if (!Number.isFinite(size)) return 72;
  return Math.min(120, Math.max(48, Math.round(size / 4) * 4));
}
