<script lang="ts">
  import * as chat from '$lib/stores/chat.svelte';
  import { getDevices, deviceState, deviceDisplayName, peerDisplayName } from '$lib/stores/devices.svelte';
  import Hint from '$lib/components/Hint.svelte';
  import type { ConvoMode, PeerDevice } from '$lib/types';

  /**
   * 会话创建 / 编辑框（原型 `#recipDialog`）：**先选模式，再选设备，最后命名**。
   *
   * 只在这一层选人，选完把 `(设备, 模式, 名字)` 交回发起方（`stores/chat` 里那份请求的
   * `onConfirm`）——「新建会话」「添加」「编辑群发」「编辑群聊」共用同一个框，几条路各写一套
   * 的话，标题、说明、按钮文案迟早分叉（原型也正是拿这一个框按模式与台数改写自己）。
   *
   * 编辑已有会话时 `lock` 有值：**模式只读**（换了模式，历史就成了两半），标题变成「编辑…」。
   * 组件由 App 在 `recipientReq` 非空时挂载，所以种子只在挂载那一刻读一次就够。
   *
   * **两张模式卡各自只写名字**：规则（单向 / 共享）收进名字旁边那颗「?」
   * —— 展开在卡片里的话，两张卡就把整个框撑满了（见 `Hint`）。
   */
  const req = chat.getRecipientReq();
  const devices = $derived(getDevices());

  // 种子只在挂载那一刻读一次（组件由 App 在 `recipientReq` 非空时挂载）：设备已经不在
  // 列表里的那些不算数（原型同款过滤）。
  let selected = $state<string[]>((req?.seed ?? []).filter((id) => getDevices().some((d) => d.id === id)));
  let picked = $state<ConvoMode>(req?.lock === 'group' ? 'group' : 'broadcast');
  let name = $state(req?.name ?? '');

  /** 编辑已有会话 = 模式锁死。 */
  const locked = $derived(req?.lock ?? null);
  const mode = $derived<ConvoMode>(locked ?? (selected.length > 1 ? picked : 'direct'));
  const multi = $derived(selected.length > 1 || !!locked);
  const isBc = $derived((locked ?? mode) === 'broadcast');

  /** 标题 / 按钮文案由"模式 + 台数"决定（原型 `syncRecipDialog`）；模式各自的规则在卡片那颗「?」里。 */
  const title = $derived(locked
    ? (isBc ? '编辑群发' : '编辑群聊')
    : multi
      ? (isBc ? '新建群发' : '新建群聊')
      : '新建会话');

  const confirmLabel = $derived(multi ? (isBc ? '进入群发' : '进入群聊') : '进入会话');

  /** 名字回落到什么：群发是「未命名群发」，群聊是参与者名字（与 `stores/chat` 的标题同一套）。 */
  const namePreview = $derived(
    isBc
      ? `群发 · ${name.trim() || '未命名群发'}`
      : `群聊 · ${name.trim() || selected.map((id) => peerDisplayName(id)).join('、') || '设备名称'}`,
  );

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
    done?.([...selected], mode, name.trim());
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="overlay open"
  role="dialog"
  aria-modal="true"
  aria-label={title}
  tabindex="-1"
  onclick={(e) => {
    if (e.target === e.currentTarget) chat.closeRecipients();
  }}
>
  <div class="dialog">
    <h3>{title}</h3>
    <p class="d-desc">勾选一台设备时为单独会话；勾选两台以上才启用模式。</p>

    <!-- 消息模式：勾选两台以上才成立；编辑已有会话时只读。规则收在名字那颗「?」里 ——
         卡片只留名字，两张卡才不会把这个框撑满。 -->
    <div class="mode-pick" data-off={locked ? '1' : '0'} role="radiogroup" aria-label="消息模式">
      <label class="mode-opt" data-mode="broadcast">
        <input
          type="radio"
          name="convoMode"
          value="broadcast"
          checked={picked === 'broadcast'}
          disabled={!!locked}
          onchange={() => (picked = 'broadcast')}
        />
        <span class="mo-ic" aria-hidden="true">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M18.5 5.5 6.5 9.5H5a1.5 1.5 0 0 0-1.5 1.5v2A1.5 1.5 0 0 0 5 14.5h1.5l12 4V5.5Z" /><path d="M8 14.8V18a1.6 1.6 0 0 0 1.6 1.6h1.2" /></svg>
        </span>
        <span class="mo-name">
          单向群发<Hint>
            只显示你发出的消息；每台设备各自收到一条单独的一对一消息 —— 互相看不到，
            对方的回复也不会回到这里（各自回到它和你的会话里）。
            <span class="hl">每台设备要分别点一次同意</span>（已配对的直接收）。
          </Hint>
        </span>
      </label>
      <label class="mode-opt" data-mode="group">
        <input
          type="radio"
          name="convoMode"
          value="group"
          checked={picked === 'group'}
          disabled={!!locked}
          onchange={() => (picked = 'group')}
        />
        <span class="mo-ic" aria-hidden="true">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1" /><circle cx="9" cy="7" r="3" /><path d="M22 19v-1a4 4 0 0 0-3-3.87" /><path d="M16 4.13a4 4 0 0 1 0 7.75" /></svg>
        </span>
        <span class="mo-name">
          共享群聊<Hint>
            群里任意一台设备发送的消息，所有参与者都能看到（同一条消息流）。
            <span class="hl">收到邀请的人点「加入」才算这个群的一员</span>；退群只影响自己
            （本机退出时这份记录也一并删掉，其他成员不受影响）。
          </Hint>
        </span>
      </label>
    </div>
    {#if locked}
      <p class="mode-note">模式不可更改（改了历史就成了两半），移除到只剩一台时会退回单独会话。</p>
    {/if}

    {#if multi}
      <div class="field">
        <label for="convoName">{isBc ? '群发名称' : '群聊名称'}</label>
        <input
          id="convoName"
          class="input"
          bind:value={name}
          maxlength="16"
          placeholder={isBc ? '例如：发布通知' : '留空则显示设备名称'}
          autocomplete="off"
        />
        <p class="field-hint">会话标题显示为「<b>{namePreview}</b>」</p>
      </div>
    {/if}

    <div class="recip-tools">
      <button class="btn btn-ghost btn-sm" type="button" onclick={toggleAll}>全选 / 取消全选</button>
      <span class="num">已选 {selected.length} 台</span>
    </div>
    <div class="recip-list" role="group" aria-label="局域网设备">
      {#each devices as d (d.id)}
        <!-- 状态点与状态章和设备页同一套（`stores/devices` 的 `deviceState`）：颜色=能不能送到、形状=有没有长连接。 -->
        {@const st = deviceState(d)}
        <label class="recip-row">
          <input type="checkbox" checked={selected.includes(d.id)} onchange={(e) => toggle(d.id, e.currentTarget.checked)} />
          <span class="d-avatar">{(deviceDisplayName(d).trim().charAt(0) || '?').toUpperCase()}</span>
          <span class="rr-main">
            <span class="rr-name">{deviceDisplayName(d)}</span>
            <span class="rr-ip num">{d.ip}:{d.port}</span>
          </span>
          <span class="state-chip {st.chip}"><span class="dot {st.dot}"></span>{st.label}</span>
        </label>
      {:else}
        <p class="field-hint">还没有设备：先到「设备」页添加，或等局域网里的设备出现。</p>
      {/each}
    </div>
    <div class="d-acts">
      <button class="btn btn-secondary" type="button" onclick={() => chat.closeRecipients()}>取消</button>
      <button class="btn btn-primary" type="button" disabled={selected.length === 0} onclick={confirm}>{confirmLabel}</button>
    </div>
  </div>
</div>