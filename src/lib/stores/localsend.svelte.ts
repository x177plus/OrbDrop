import { listen } from '@tauri-apps/api/event';
import * as api from '$lib/api';
import type { LocalSendStatus } from '$lib/types';
import { t } from '$lib/i18n.svelte';

/**
 * LocalSend 兼容面的**运行时状态**（桌面设置面板与移动设置页共用）。
 *
 * 与 `getSettings().localsendEnabled` 是两件事：那个是用户意图，这个是运行结果。
 * 状态归后端（`localsend_status` / `localsend://status`），这里只做“拿一次初值 +
 * 跟着事件走”，两端看到的是同一份。
 */
let status = $state<LocalSendStatus | null>(null);

const ready: Promise<void> = (async () => {
  try {
    await listen<LocalSendStatus>('localsend://status', (ev) => {
      status = ev.payload;
    });
  } catch (e) {
    console.error('LocalSend status listener failed:', e);
  }
  try {
    status = await api.localsendStatus();
  } catch (e) {
    console.error('LocalSend status load failed:', e);
  }
})();

export function getLocalSendStatus(): LocalSendStatus | null {
  return status;
}

/** 进设置页时再拉一次（后端可能刚被别的窗口/事件改过）。 */
export async function refreshLocalSendStatus(): Promise<void> {
  await ready;
  try {
    status = await api.localsendStatus();
  } catch (e) {
    console.error('LocalSend status refresh failed:', e);
  }
}

/**
 * 立即开/关。**只有写盘失败才 reject**；53317 被占用、本机离线都只是返回状态里的
 * 降级/暂停，调用方不要把开关弹回去。
 */
export async function setLocalSendEnabled(enabled: boolean): Promise<LocalSendStatus> {
  const next = await api.setLocalsendEnabled(enabled);
  status = next;
  return next;
}

/**
 * 设置页「重试」：只叫醒后台 guard 补一拍，**不重启**（在途传输不断）。
 * 不碰本地状态：guard 的结果由 `localsend://status` 推，避免旧快照覆盖新事件。
 */
export async function retryLocalSendNow(): Promise<void> {
  await api.localsendRetry();
}

export type LocalSendTone = 'ok' | 'warn' | 'error' | 'muted';

export interface LocalSendStatusView {
  tone: LocalSendTone;
  text: string;
  /** 需不需要给一颗「重试」（自动恢复仍在跑，按钮是让用户立刻试一次）。 */
  retry: boolean;
}

/**
 * 状态行的人话。**开关关着时返回 `null`**（设置页不画状态行）。
 *
 * 五档对应后端 `StandardPortState` + 发现/离线两个维度；文案里刻意区分
 * “降级（官方仍可互通）”与“发现没起来”，不要让用户以为整个功能坏了。
 */
export function localSendStatusView(s: LocalSendStatus | null): LocalSendStatusView | null {
  if (!s || !s.enabled) return null;
  if (!s.online) {
    return {
      tone: 'muted',
      text: t('已开启；本机离线时自动暂停，上线后恢复。'),
      retry: false,
    };
  }
  if (!s.discovery) {
    return {
      tone: 'error',
      text: t('发现服务未启动：{error}。仍会从临时端口公告，端口空出后自动恢复。', {
        error: s.discoveryError ?? t('未知原因'),
      }),
      retry: true,
    };
  }
  if (s.standardPort === 'stopped' && s.advertisedPort === 0) {
    // 主 HTTP 还没绑上：这一档没有端口守护任务会来叫醒它（`start` 在 port == 0 时
    // 直接返回），只能让用户点「重试」—— 不能显示成"下一拍就会起"。
    return { tone: 'muted', text: t('正在启动…'), retry: true };
  }
  switch (s.standardPort) {
    case 'bound':
      return { tone: 'ok', text: t('运行中：已监听 53317。'), retry: false };
    case 'mainIsStandard':
      return { tone: 'ok', text: t('运行中：主端口就是 53317。'), retry: false };
    case 'busy':
      return {
        tone: 'warn',
        text: t(
          '53317 被占用（本机同时运行 LocalSend 时常见）：已改用主端口 {port} 提供兼容；官方 LocalSend 仍可互通，写死 53317 的第三方暂时发不进来。',
          { port: s.advertisedPort },
        ),
        retry: true,
      };
    case 'failed':
      return {
        tone: 'warn',
        text: t('53317 独立监听启动失败：已回落到主端口 {port}；官方 LocalSend 仍可互通。', {
          port: s.advertisedPort,
        }),
        retry: true,
      };
    case 'stopped':
      return {
        tone: 'warn',
        text: t('兼容层没有在运行，稍后会自动重试。'),
        retry: true,
      };
  }
}