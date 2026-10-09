<script lang="ts">
  import { onMount } from 'svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { wipeCredentials, relayTraffic } from '$lib/api';
  import Hint from '$lib/components/Hint.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import { getSettings, loadSettings, updateSettings } from '$lib/stores/settings.svelte';
  import { formatFileSize } from '$lib/types';
  import type { OrbShape, RectPage, RelayTraffic } from '$lib/types';
  import Orb from '$lib/components/Orb.svelte';
  import RectOrb from '$lib/components/RectOrb.svelte';

  /**
   * 缩略图里的骨架数据：**只有形状，没有内容** —— 名字 / 地址 / 元信息一律空串，卡片上那些
   * 位置由预览样式画成灰条（见 main.css 的「悬浮形态缩略图」一节）；项数与角标数字是组件自己
   * 生成的，也一并隐掉。看的人只需要认出"这是张卡片，里面有图标和文字位，右边有分页点"。
   * 三页是因为**只有一页时分页点整列不画**：要看全分页点，缩略图就得有得翻。
   */
  const PREVIEW_PAGES: RectPage[] = [
    {
      id: 'preview-1', kind: 'peer', name: '', ip: '', loading: false, error: null, dir: null, up: null,
      // 前两格当目录、其余当文件：骨架里两种图标（文件夹 / 文件）都能看到
      items: [
        { id: 'preview-1-a', path: '', name: '', isDir: true, meta: '' },
        { id: 'preview-1-b', path: '', name: '', isDir: true, meta: '' },
        { id: 'preview-1-c', path: '', name: '', isDir: false, meta: '' },
        { id: 'preview-1-d', path: '', name: '', isDir: false, meta: '' },
        { id: 'preview-1-e', path: '', name: '', isDir: false, meta: '' },
      ],
    },
    {
      id: 'preview-2', kind: 'peer', name: '', ip: '', loading: false, error: null, dir: null, up: null,
      items: [
        { id: 'preview-2-a', path: '', name: '', isDir: true, meta: '' },
        { id: 'preview-2-b', path: '', name: '', isDir: false, meta: '' },
      ],
    },
    { id: 'preview-3', kind: 'peer', name: '', ip: '', loading: false, error: null, dir: null, up: null, items: [] },
  ];

  let {
    open = false,
    onClose,
    onToast,
  }: {
    open?: boolean;
    onClose: () => void;
    onToast: (msg: string) => void;
  } = $props();

  // Local form state
  let port = $state(3890);
  let downloadDir = $state('');
  let downloadMode = $state<'Direct' | 'AskEachTime'>('Direct');
  let deviceName = $state('');
  let autoStart = $state(true);
  /**
   * 新添加的目录/文件默认开播（后端 `AppSettings.autoBroadcastNew`，默认关）。
   * 只影响之后新登记的共享项，已在列表里的开关不受影响。
   */
  let autoBroadcastNew = $state(false);
/**
   * 允许本机替别人中转（后端 `AppSettings.allowRelay` + `groupRelay`，默认关）。
   *
   * **一个开关管住"本机替别人做事"的两条路**：① 替其它设备盲转字节（两台设备单播不通但
   * 都连得上本机时，本机替它们搬字节，两端 TLS 端到端不变、本机看不到内容）；② 替够不着的
   * 同群成员转一份群消息（最多 3 跳）。代价都是本机的上行带宽 —— 所以默认关，由用户自己选。
   *
   * 「记住并自动复用跨网段中转」（`relayMemory`）不在这颗开关里：它不替别人做事、也不向对端
   * 暴露本机的新能力，只是本机自己的选路提示，**默认开、不给控制**（保存时一律写成开）。
   */
  let relay = $state(false);
  /** 本机替别人中转的累计流量（`relay_traffic`）：只读的用量记录，打开面板时现拉。 */
  let traffic = $state<RelayTraffic | null>(null);
  /**
   * 接收推送时一次跑几路传输（后端 `AppSettings.transferWorkers`，默认 4，取值域 1..=8）。
   */
  let transferWorkers = $state(4);
  /** 大文件的分块大小（MB，后端 `AppSettings.transferBlockMb`，默认 4，取值域 1..=32）。 */
  let transferBlockMb = $state(4);
  let bcExePath = $state('');
  /** 折叠态悬浮形态：圆球（现有的 62px 球）/ 圆角矩形悬浮框（设备分页卡片）。 */
  let orbShape = $state<OrbShape>('circle');
  let saving = $state(false);
  /** 「清空全部凭据」的二次确认：点一下变成确认条，再点才真删。 */
  let wipeArmed = $state(false);
  let wiping = $state(false);

  async function wipeNow() {
    wiping = true;
    try {
      const n = await wipeCredentials();
      wipeArmed = false;
      onToast(n > 0 ? `已清空 ${n} 台设备的凭据，需重新配对` : '本机没有配对凭据');
    } catch (e: any) {
      console.error('Wipe credentials failed:', e);
      onToast(`清空失败：${String(e)}`);
    } finally {
      wiping = false;
    }
  }

  /**
   * 系统目录选择框，选**唯一**的落盘目录（手动下载 + 推送接收）。
   * 取消（含 Esc / 关窗）时 `open` 返回 null，此时保持原值不动。
   * `defaultPath` 传当前值：打开就落在当前目录，省掉一层层点。
   */
  async function pickDownloadDir() {
    try {
      const picked = await openDialog({
        title: '选择下载目录',
        directory: true,
        multiple: false,
        defaultPath: downloadDir || undefined,
      });
      const dir = Array.isArray(picked) ? picked[0] : picked;
      if (dir) downloadDir = dir;
    } catch (e: any) {
      console.error('Pick directory failed:', e);
      onToast(`打开目录选择框失败：${String(e)}`);
    }
  }

  /**
   * 读一次"本机替别人中转了多少"（打开面板时、以及面板开着期间轮询）。
   * 它是**只读的用量记录**：不是设置项，不参与保存。
   */
  async function refreshTraffic() {
    try {
      traffic = await relayTraffic();
    } catch (e: any) {
      console.error('Relay traffic load failed:', e);
    }
  }

  onMount(() => {
    loadSettings().then(() => {
      const s = getSettings();
      if (s) {
        port = s.port;
        downloadDir = s.downloadDir;
        downloadMode = s.downloadMode;
        deviceName = s.deviceName;
        autoStart = s.autoStart;
        autoBroadcastNew = s.autoBroadcastNew;
        relay = s.allowRelay || s.groupRelay;
        transferWorkers = s.transferWorkers;
        transferBlockMb = s.transferBlockMb;
        bcExePath = s.bcExePath ?? '';
        orbShape = s.orbShape;
      }
    });
  });

  // 「中转流量」是**实时视图**：本机替别人搬的字节随时在涨（长连接中途也会涨）。而本组件从
  // 应用启动起就常驻 DOM，原先只在 `onMount` 读一次的话，用户打开设置看到的永远是**启动
  // 那一刻**的快照。中转没有可订阅的界面事件，面板开着时按 5s 轮询，关掉即停。
  $effect(() => {
    if (!open) return;
    void refreshTraffic();
    const t = setInterval(() => void refreshTraffic(), 5000);
    return () => clearInterval(t);
  });

  /**
   * 数字输入框（传输的两项）的取值：进保存前一律取整并夹进后端的取值域。
   * 输入框可以被手填、清空或贴进任意文本 —— 清空时 `bind:value` 给的是 `null`
   * （Svelte 对 `<input type="number">` 的空值就是这么转的），非数字给 `undefined`，
   * 两种都退回默认值，不把"清空"悄悄写成下限。
   */
  function clampNum(v: number | null | undefined, lo: number, hi: number, fallback: number): number {
    if (v == null) return fallback;
    const n = Math.trunc(Number(v));
    return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : fallback;
  }

  async function save() {
    saving = true;
    try {
      await updateSettings({
        port,
        downloadDir,
        downloadMode,
        deviceName,
        autoStart,
        autoBroadcastNew,
        // 一颗开关管两条路：替其它设备盲转字节 + 替群聊转消息（`allowRelay` / `groupRelay`）。
        groupRelay: relay,
        allowRelay: relay,
        // 「记住并自动复用跨网段中转」不给控制、默认开：一律写开。
        relayMemory: true,
        transferWorkers: clampNum(transferWorkers, 1, 8, 4),
        transferBlockMb: clampNum(transferBlockMb, 1, 32, 4),
        bcExePath,
        orbShape,
      });
      onClose();
      onToast('设置已保存');
    } catch (e: any) {
      console.error('Settings save failed:', e);
      onToast(`保存失败：${String(e)}`);
    } finally {
      saving = false;
    }
  }

  function cancel() {
    // Reload values from store (discard edits)
    const s = getSettings();
    if (s) {
      port = s.port;
      downloadDir = s.downloadDir;
      downloadMode = s.downloadMode;
      deviceName = s.deviceName;
      autoStart = s.autoStart;
      autoBroadcastNew = s.autoBroadcastNew;
      relay = s.allowRelay || s.groupRelay;
      transferWorkers = s.transferWorkers;
      transferBlockMb = s.transferBlockMb;
      bcExePath = s.bcExePath ?? '';
      orbShape = s.orbShape;
    }
    onClose();
  }
</script>

<section class="sheet" class:open aria-label="设置">
  <header class="sheet-head">
    <span class="panel-brand" style="margin-right:auto">
      <Logo size={20} />设置
    </span>
    <button class="icon-btn" onclick={cancel} aria-label="关闭设置">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
        <path d="M6 6l12 12M18 6 6 18"/>
      </svg>
    </button>
  </header>
  <div class="sheet-body">
    <div class="field">
      <label for="deviceName">设备名称</label>
      <input class="input" id="deviceName" bind:value={deviceName} />
    </div>
    <div class="field">
      <label for="downloadDir">下载目录<Hint>
        手动「下载到本地」和对端「推送」接收都默认存这里（选了「每次询问」时，接收推送那条
        会先让你挑一个目录，挑的那个就是这次接收的根）；推送一律落在 <strong>发送方名</strong>
        目录下，按你选中的那些项各自的名字展开（不会和下载的文件混在一层）。
      </Hint></label>
      <div class="path-input">
        <input class="input" id="downloadDir" value={downloadDir} readonly />
        <button class="btn btn-secondary btn-sm" onclick={pickDownloadDir}>选择</button>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label id="dlModeLabel">下载模式</label>
      <div class="radio-group">
        <label class="radio">
          <input type="radio" name="dlMode" bind:group={downloadMode} value="Direct" />
          <span class="r-label">直接下载<span class="r-sub">收下文件并保存到下载目录（不问）</span></span>
        </label>
        <label class="radio">
          <input type="radio" name="dlMode" bind:group={downloadMode} value="AskEachTime" />
          <span class="r-label">每次询问<span class="r-sub">下载、以及接收未配对设备的推送时，都先弹窗选位置</span></span>
        </label>
      </div>
    </div>
    <div class="field">
      <label for="port">服务端口<Hint>被占用时自动 +1 探测，实际端口显示在底部状态栏。</Hint></label>
      <input class="input num" id="port" type="number" bind:value={port} />
    </div>
    <div class="field">
      <label for="transferWorkers">传输并发数<Hint>
        一次接收同时跑几路传输（一块 / 一个文件算一路）。局域网上 4 就够把千兆喂饱，
        调到 <strong>8</strong> 以上只会把连接、句柄与磁盘队列都堆起来，反而互相拖。
        取值 <strong>1–8</strong>，超出范围会在保存时夹住。
      </Hint></label>
      <input class="input num" id="transferWorkers" type="number" min="1" max="8" bind:value={transferWorkers} />
    </div>
    <div class="field">
      <label for="transferBlockMb">分块大小（MB）<Hint>
        大文件按它切片、片与片之间并行。<strong>4MB</strong> 在千兆下约 33ms 一块 ——
        取消 / 失败的可感知延迟就在这个尺度，同时「一块要重传」的代价也不大。
        取值 <strong>1–32</strong>，超出范围会在保存时夹住；再大就不是并行，而是「一次传半个文件」。
      </Hint></label>
      <input class="input num" id="transferBlockMb" type="number" min="1" max="32" bind:value={transferBlockMb} />
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label id="orbShapeLabel">悬浮形态<Hint>
        收起后常驻屏幕上的那件东西长什么样。缩略图就是实物外观（角标、分页点都在），
        里面的名字是示意用的骨架。<br />
        <strong>圆球</strong>：62px 的球，贴边会收进屏幕边缘、悬停滑回来。<br />
        <strong>圆角矩形悬浮框</strong>：一张卡片，第一页是本机、其后每台长连接设备一页，
        列出各自对本机可见的共享项；贴边会收起成一条竖条（图标 + 设备名 + ip:port），
        悬停滑回来；直接拖动卡片里的条目可以拖到资源管理器里（与面板里一样），
        滚轮/点右侧小点换页。
      </Hint></label>
      <div class="shape-picker" role="radiogroup" aria-labelledby="orbShapeLabel">
        <label class="shape-opt" title="圆球：贴边收起半颗，悬停滑回来">
          <input type="radio" name="orbShape" bind:group={orbShape} value="circle" aria-label="圆球" />
          <!-- 缩略图 = **真组件 + 骨架数据**（同一套 CSS/DOM，件件与实物一致），外面套一层
               缩放框（`--s`/`--w`/`--h` = 缩放比与含角标出格在内的视觉外框）。
               圆球那一档放大到与卡片同高（两个缩略图并排，等高才像同一层级的两个选择），
               卡片那一档缩到 0.78 才装得进半栏。
               整块 `inert`：缩略图不吃指针、不进 Tab 序，也不会被拖出去。
               说明文字不放在图下面（只留两张图）：两档各自是什么，看上方那颗「?」。 -->
          <span class="so-frame" style="--s:1.5; --w:72px; --h:72px" inert>
            <span class="so-scale">
              <Orb badgeCount={3} notifCount={3} expanded={false} onToggle={() => {}} onDragStart={() => {}} />
            </span>
          </span>
        </label>
        <label class="shape-opt" title="圆角矩形悬浮框：设备分页卡片，贴边收起成竖条，可直接拖出共享项">
          <input type="radio" name="orbShape" bind:group={orbShape} value="rect" aria-label="圆角矩形悬浮框" />
          <span class="so-frame" style="--s:0.78; --w:211px; --h:139px" inert>
            <span class="so-scale">
              <RectOrb
                pages={PREVIEW_PAGES}
                deviceCount={PREVIEW_PAGES.length}
                notifCount={3}
                expanded={false}
                onToggle={() => {}}
                onDragStart={() => {}}
                onActivate={() => {}}
                onUp={() => {}}
                onContext={() => {}}
                onToast={() => {}}
              />
            </span>
          </span>
        </label>
      </div>
    </div>
    <div class="field">
      <label for="bcExePath">BC 可执行文件路径<Hint>
        用于右键菜单里的「用 BeyondCompare 比较」，例如 BCompare.exe 的完整路径。
      </Hint></label>
      <input class="input" id="bcExePath" bind:value={bcExePath} placeholder="留空 = 自动探测" />
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>开机自启</label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">登录后自动启动 OrbDrop</span>
        <label class="switch">
          <input type="checkbox" bind:checked={autoStart} data-autostart />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>添加后自动共享<Hint>
        <strong>打开时</strong>：新添加的目录 / 文件自动打开广播开关，当场就对长连接设备可见
        （与每一项自己那个开关是同一个东西，加完随时可以单独关掉）。<br />
        <strong>默认关闭</strong>，只作用于<strong>之后</strong>添加的：已在列表里的共享项开关
        一个都不动 —— 手动改过的不会被这里翻回来。
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">新添加的目录/文件自动开启共享</span>
        <label class="switch">
          <input type="checkbox" bind:checked={autoBroadcastNew} data-autobroadcast />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>允许中转<Hint>
        打开后本机替别人做事，两条路都在这**一个**开关里：<br />
        <strong>① 替其它设备盲转字节</strong>：两台设备在同一张 Wi-Fi 上互相连不上时（有些 AP
        会把客户端之间的流量隔开），本机连着 A 也连着 B，就替它们搬字节 —— 能直连就直连，
        数据端到端加密，本机只搬字节、看不到内容。<strong>不限已配对设备</strong>：开着就允许，
        同网段里能到达本机的任意设备都可以借（强制保留的只有"到对方给的那个地址核实目标身份"
        这一关，少了它本机就成了给任意局域网地址开隧道的工具）。<br />
        <strong>② 替群聊中转消息</strong>：群里两个成员互相够不着时（跨网段、只有本机跟它们都
        连得上），本机替它们把消息转一份，最多 3 跳，同一份转过一次就不再转（不来回绕圈）。<br />
        <strong>默认关闭</strong>：两条路都占本机上行带宽 —— 这笔开销该不该出由你定；对端之间
        直连得了时，这个开关开不开都一样。关闭会<strong>立刻收掉</strong>本机正在替别人转的每一条。
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">本机替够不着彼此的其它设备搬字节、替够不着的同群成员转消息；会占用本机上行带宽</span>
        <label class="switch">
          <input type="checkbox" bind:checked={relay} data-relay />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- 中转流量（F-106 / S-09）：替别人搬了多少 —— 只读的累计记录，不随保存写。 -->
    <div class="field">
      <label>中转流量<Hint>
        本机替别人中转<strong>累计搬过的数据量</strong>（收发两个方向之和）与连接数 —— 中转方的
        代价就是上行带宽，这里就是这笔账。<br />
        只统计<strong>本机当中转方</strong>替别人搬的那部分：替其它设备<strong>盲转的字节</strong>，
        以及替群聊<strong>转发出去的消息</strong>。本机自己发出去、经别人中转，不在这个数里。<br />
        连接数只数盲转的连接（消息中转不算一条连接）。它是落了盘的记录（`config.toml` 的
        `relayBytesTotal` / `relayConnsTotal`），重启接着算。
      </Hint></label>
      {#if traffic && (traffic.bytes > 0 || traffic.connections > 0)}
        <span style="font-size:var(--fs-sm); color:var(--muted);">
          累计 {formatFileSize(traffic.bytes)}{traffic.connections > 0 ? ` · ${traffic.connections} 条连接` : ''}
        </span>
      {:else}
        <span style="font-size:var(--fs-sm); color:var(--muted);">还没有替别人中转 —— 有设备借本机中转之后才会有</span>
      {/if}
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>配对凭据<Hint>
        全部设备的 token 存在本机数据目录的一个文件里（仅遮挡、非加密）。这里是唯一的删除入口：
        清空后所有设备回到「未配对」需重新配对，已写进 Beyond Compare 的 WebDAV profile 也一并清掉。
      </Hint></label>
      {#if wipeArmed}
        <div style="display:flex; align-items:center; gap:8px;">
          <span style="font-size:var(--fs-sm); color:var(--muted);">确认清空？所有设备需重新配对</span>
          <button class="btn btn-ghost btn-sm" onclick={() => (wipeArmed = false)} disabled={wiping}>取消</button>
          <button class="btn btn-primary btn-sm" onclick={wipeNow} disabled={wiping}>
            {wiping ? '清空中…' : '确认清空'}
          </button>
        </div>
      {:else}
        <button class="btn btn-secondary btn-sm" onclick={() => (wipeArmed = true)}>清空全部凭据</button>
      {/if}
    </div>
  </div>
  <footer class="sheet-foot">
    <button class="btn btn-ghost" onclick={cancel}>取消</button>
    <button class="btn btn-primary" onclick={save} disabled={saving}>
      {saving ? '保存中…' : '保存设置'}
    </button>
  </footer>
</section>