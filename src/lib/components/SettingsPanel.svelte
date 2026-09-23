<script lang="ts">
  import { onMount } from 'svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { wipeCredentials } from '$lib/api';
  import Hint from '$lib/components/Hint.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import { getSettings, loadSettings, updateSettings } from '$lib/stores/settings.svelte';
  import type { OrbShape, RectPage } from '$lib/types';
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
   * 替群聊中转消息（后端 `AppSettings.groupRelay`，默认关）。开着才在本机替够不着的同群成员
   * 转一份（最多 3 跳），代价是本机的上行带宽 —— 所以默认关，由用户自己选。
   */
  let groupRelay = $state(false);
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
        groupRelay = s.groupRelay;
        bcExePath = s.bcExePath ?? '';
        orbShape = s.orbShape;
      }
    });
  });

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
        groupRelay,
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
      groupRelay = s.groupRelay;
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
        手动「下载到本地」和对端「推送」接收都存这里；推送落在 <strong>发送方名</strong>
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
          <span class="r-label">直接下载<span class="r-sub">收下文件并保存到下载目录</span></span>
        </label>
        <label class="radio">
          <input type="radio" name="dlMode" bind:group={downloadMode} value="AskEachTime" />
          <span class="r-label">每次询问<span class="r-sub">手动下载时每次弹窗选保存位置</span></span>
        </label>
      </div>
    </div>
    <div class="field">
      <label for="port">服务端口<Hint>被占用时自动 +1 探测，实际端口显示在底部状态栏。</Hint></label>
      <input class="input num" id="port" type="number" bind:value={port} />
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
      <label>允许为群聊中转消息<Hint>
        群里两个成员互相够不着时（跨网段、只有本机跟它们都连得上），本机替它们把消息转一份，
        最多 3 跳；同一份消息转过一次就不再转（不来回绕圈）。<br />
        <strong>默认关闭</strong>：转一份就占一份本机的上行带宽，而这笔开销该不该出，由你来定 ——
        对端之间直连得了时，这个开关开不开都一样。
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">开启后本机会把同群消息转给其他成员（最多 3 跳），用于跨网段；会占用本机上行带宽</span>
        <label class="switch">
          <input type="checkbox" bind:checked={groupRelay} data-grouprelay />
          <span class="track"></span>
        </label>
      </div>
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