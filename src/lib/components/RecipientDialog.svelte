<script lang="ts">
  import * as chat from '$lib/stores/chat.svelte';
  import { getDevices } from '$lib/stores/devices.svelte';
  import type { PeerDevice } from '$lib/types';

  /**
   * 收件人选择框（原型 `#recipDialog`）：勾一台 = 一对一会话，勾两台以上 = 群会话。
   *
   * 只在这一层选人，选完把结果交回发起方（`stores/chat` 里那份请求的 `onConfirm`）——
   * 「新建会话」与详情里的「添加」共用同一个框，两条路各写一套的话，选中规则迟早分叉。
   * 组件由 App 在 `recipientReq` 非空时挂载，所以种子只在挂载那一刻读一次就够。
   */
  const req = chat.getRecipientReq();
  const devices = $derived(getDevices());
  // 种子只在挂载那一刻读一次（组件由 App 在 `recipientReq` 非空时挂载）：设备已经不在
  // 列表里的那些不算数（原型同款过滤）。
  let selected = $state<string[]>((req?.seed ?? []).filter((id) => getDevices().some((d) => d.id === id)));

  /** 状态章（与设备页同一套三档：长连接 / 已配对 / 未绑定；离线的再把边框打虚线）。 */
  function chipOf(d: PeerDevice): { cls: string; dot: string; label: string } {
    if (d.connected) return { cls: 'state-chip connected', dot: 'dot ok', label: '长连接' };
    if (!d.paired) return { cls: 'state-chip unbound', dot: 'dot off', label: '未绑定' };
    return { cls: `state-chip paired${d.online ? '' : ' offline'}`, dot: 'dot off', label: '已配对' };
  }

  function toggle(id: string, on: boolean) {
    if (on) {
      if (!selected.includes(id)) selected = [...selected, id];
    } else {
      selected = selected.filter((x) => x !== id);
    }
  }

  function toggleAll() {
    selected = selected.length === devices.length ? [] : devices.map((d) => d.id);
  }

  function confirm() {
    if (!selected.length) return;
    const done = req?.onConfirm;
    chat.closeRecipients();
    done?.([...selected]);
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="overlay open"
  role="dialog"
  aria-modal="true"
  aria-label="选择收件人"
  tabindex="-1"
  onclick={(e) => {
    if (e.target === e.currentTarget) chat.closeRecipients();
  }}
>
  <div class="dialog">
    <h3>选择收件人</h3>
    <p class="d-desc">勾选一台设备是一对一会话，勾选两台以上会创建一个群会话。</p>
    <div class="recip-tools">
      <button class="btn btn-ghost btn-sm" type="button" onclick={toggleAll}>全选 / 取消全选</button>
      <span class="num">已选 {selected.length} 台</span>
    </div>
    <div class="recip-list" role="group" aria-label="局域网设备">
      {#each devices as d (d.id)}
        {@const chip = chipOf(d)}
        <label class="recip-row">
          <input type="checkbox" checked={selected.includes(d.id)} onchange={(e) => toggle(d.id, e.currentTarget.checked)} />
          <span class="d-avatar">{(d.name.trim().charAt(0) || '?').toUpperCase()}</span>
          <span class="rr-main">
            <span class="rr-name">{d.name}</span>
            <span class="rr-ip num">{d.ip}:{d.port}</span>
          </span>
          <span class={chip.cls}><span class={chip.dot}></span>{chip.label}</span>
        </label>
      {/each}
    </div>
    <div class="d-acts">
      <button class="btn btn-secondary" type="button" onclick={() => chat.closeRecipients()}>取消</button>
      <button class="btn btn-primary" type="button" disabled={selected.length === 0} onclick={confirm}>确定</button>
    </div>
  </div>
</div>