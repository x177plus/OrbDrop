<script lang="ts">
  import * as chat from '$lib/stores/chat.svelte';
  import { getSettings } from '$lib/stores/settings.svelte';

  /**
   * 接收方视角预览（原型 `#peekDialog`）：一条群发记录在**某台设备**那边长什么样。
   *
   * 这个框的存在理由是**把"单向"这件事说清楚**：群发的记录里那一条条，在每台设备那边都是
   * 一条单独的一对一消息 —— 它看不到别的收件人，也看不到你发给别人的内容。所以这里画的就是
   * 对方那条会话的样子（一颗头像、一个气泡、一行"当时送到没有"），可以逐台翻。
   *
   * 数据全在 `chat.peekView()` 里：按**这条消息自己当时的 `targets`** 逐台走（不是名单的当前
   * 成员），状态也是那一刻的真实状态（已送达 / 发送中 / 等待发送 / 未送达）。
   */
  const view = $derived(chat.peekView());
  const myName = $derived(getSettings()?.deviceName?.trim() || '本机');

  /** 那一刻的状态怎么说（一句人话，不是状态码）。 */
  const outcome = $derived.by(() => {
    const v = view;
    if (!v) return { text: '', bad: false };
    if (v.state === 'ok') return { text: '该设备已收到这条消息', bad: false };
    if (v.state === 'pending') return { text: `这条还在路上（${v.stateLabel}）`, bad: false };
    // 排队等对端上线：它不在这台设备上，但**消息没丢** —— 所以不能读作"没收到"。
    if (v.state === 'waiting') {
      return { text: `这台设备那会儿不在网上，这条排着队等它上线（${v.stateLabel}）`, bad: false };
    }
    return { text: `该设备没有收到这条消息（${v.stateLabel}）`, bad: true };
  });
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="overlay open"
  role="dialog"
  aria-modal="true"
  aria-label="接收方看到的样子"
  tabindex="-1"
  onclick={(e) => {
    if (e.target === e.currentTarget) chat.closePeek();
  }}
>
  <div class="dialog">
    <h3>接收方看到的样子</h3>
    <p class="d-desc">群发是单向的：每台设备只收到一条单独的一对一消息。</p>
    {#if view}
      <div class="peek-frame">
        <header class="peek-head">
          <span class="d-avatar">{view.initial}</span>
          <span class="peek-who">
            <span class="pk-nm">{view.deviceName}</span>
            <span class="pk-sub">单独会话 · 来自 {myName}</span>
          </span>
          <span class="peek-badge">预览</span>
        </header>
        <div class="peek-thread">
          <div class="peek-msg">
            <div class="peek-bubble">
              {#if view.body.kind === 'file'}
                <span class="pb-file">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z" /></svg>
                </span>
                <span class="pb-main">
                  <span class="pb-name">{view.body.name}</span>
                  <span class="pb-size">{view.body.size}</span>
                </span>
              {:else}
                {view.body.text}
              {/if}
            </div>
            <div class="peek-meta">{view.time}</div>
          </div>
          <div class="peek-off" class:bad={outcome.bad}>{outcome.text}</div>
        </div>
      </div>
      <p class="peek-note">对方只看到这一条单独的一对一消息；群发中的其他收件人，以及你发出的其他内容，都不会出现在这里。</p>
      <div class="d-acts">
        <button class="btn btn-secondary" type="button" disabled={view.total < 2} onclick={() => chat.peekNext()}>
          {view.total > 1 ? `下一台（${view.index + 1}/${view.total}）` : '只有这一台'}
        </button>
        <button class="btn btn-primary" type="button" onclick={() => chat.closePeek()}>知道了</button>
      </div>
    {:else}
      <p class="peek-note">这条记录已经不在会话里了（设备被移除或记录被清空）。</p>
      <div class="d-acts">
        <button class="btn btn-primary" type="button" onclick={() => chat.closePeek()}>知道了</button>
      </div>
    {/if}
  </div>
</div>