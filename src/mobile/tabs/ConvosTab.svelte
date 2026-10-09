<script lang="ts">
  import * as chat from "$lib/stores/chat.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";

  // 列表的行、未读数、最近一条的摘要与时刻都在会话 store 里算好了（原型那套 `summary` /
  // `convoLastTime` / `dotState` 是 mock 自己算的，这里一律不再重算一遍）。
  const rows = $derived(chat.convoRows());
  const invites = $derived(chat.pendingGroups());

  /** 接受一条群邀请。 */
  async function join(id: string): Promise<void> {
    try {
      await chat.joinGroup(id);
    } catch (e) {
      store.failed("加入群聊", e);
      return;
    }
    // `joinGroup` 自己会打开这个群，但那次打开没走 hash 路由：不把 hash 跟上去，"返回"就落到
    // 同一个 hash 上（不触发 hashchange 事件），会话子屏会退不出去 —— 路由的真相就是 hash。
    const key = chat.activeThread()?.key;
    if (key) store.navigateChat(key);
  }

  /** 拒绝一条群邀请（后端记下"拒过"，同一份邀请不会再弹）。 */
  async function dismiss(id: string): Promise<void> {
    try {
      await chat.dismissGroup(id);
    } catch (e) {
      store.failed("拒绝群邀请", e);
    }
  }
</script>

<div class="body">
  <!-- 原型里没有"待确认的群邀请"这一档（那是 mock 没有的状态）：这里用文件页那张卡的同款类名
       拼出来 —— 一张 `.card.list` 里一行 `.frow`，右侧两颗小按钮。 -->
  {#if invites.length}
    <div class="sec">
      <div class="sec-label"><span>群邀请</span><span class="num">{invites.length} 个</span></div>
      <div class="stack">
        {#each invites as invite (invite.id)}
          {@const wait = invite.buffered > 0 ? ` · 已有 ${invite.buffered} 条消息在等你` : ''}
          <div class="card list">
            <div class="frow">
              <span class="av grp"><Icon name="group" /></span>
              <span class="fmain">
                <span class="fname2">{invite.name}</span>
                <span class="fmeta">来自 {invite.fromName} · {invite.members.length} 台设备{wait}</span>
              </span>
              <span class="dacts">
                <button class="btn btn-secondary btn-sm" type="button" onclick={() => void dismiss(invite.id)}>
                  拒绝
                </button>
                <button class="btn btn-primary btn-sm" type="button" onclick={() => void join(invite.id)}>
                  加入
                </button>
              </span>
            </div>
          </div>
        {/each}
      </div>
    </div>
  {/if}

  {#if rows.length}
    <div class="sec">
      <div class="stack">
        {#each rows as row (row.key)}
          <button
            class="convo"
            class:bc={row.mode === "broadcast"}
            type="button"
            onclick={() => store.navigateChat(row.key)}
          >
            {#if row.mode === "broadcast"}
              <span class="av bc"><Icon name="bc" /></span>
            {:else if row.mode === "group"}
              <span class="av grp"><Icon name="group" /></span>
            {:else}
              <span class="av dir">{row.initial}</span>
            {/if}
            <span class="cv-main">
              <span class="cv-top">
                <span class="cv-name">{row.label}</span>
                {#if row.mode === "broadcast"}
                  <span class="cv-mode bc"><Icon name="bc" />群发</span>
                {:else if row.mode === "group"}
                  <span class="cv-mode grp"><Icon name="group" />群聊</span>
                {/if}
                <span class="cv-dot {row.dot}"></span>
                <span class="cv-time num">{row.time}</span>
              </span>
              <span class="cv-prev">{row.sub}</span>
            </span>
            {#if row.unread}
              <span class="cv-unread num">{row.unread}</span>
            {/if}
          </button>
        {/each}
      </div>
    </div>
  {:else if !invites.length}
    <!-- 有邀请卡时不画这颗鸡蛋："先在设备页配对或连接一台设备"对着一张已经能点的邀请卡是句废话。 -->
    <div class="empty">
      <Icon name="chat" />
      <p>还没有会话</p>
      <p class="es">先在设备页配对或连接一台设备，再发起会话。</p>
    </div>
  {/if}
</div>