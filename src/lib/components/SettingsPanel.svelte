<script lang="ts">
  import { onMount } from 'svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { wipeCredentials, relayTraffic, setLanguage } from '$lib/api';
  import { t, setLanguagePreference, adoptLanguagePreference, normalizePreference, type Locale } from '$lib/i18n.svelte';
  import Hint from '$lib/components/Hint.svelte';
  import LanguageSelect from '$lib/components/LanguageSelect.svelte';
  import UpdateNotes from '$lib/components/UpdateNotes.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import { getSettings, loadSettings, updateSettings } from '$lib/stores/settings.svelte';
  import {
    getLocalSendStatus,
    localSendStatusView,
    refreshLocalSendStatus,
    retryLocalSendNow,
    setLocalSendEnabled,
  } from '$lib/stores/localsend.svelte';
  import { fileManagerLabel, isMac } from '$lib/stores/platform.svelte';
  import * as upd from '$lib/stores/update.svelte';
  import { formatFileSize, updateStatusText } from '$lib/types';
  import type { OrbShape, RectPage, RelayTraffic } from '$lib/types';
  import Orb from '$lib/components/Orb.svelte';
  import RectOrb from '$lib/components/RectOrb.svelte';

  /**
   * 缩略图里的示例数据：**真组件 + 像样的一段内容**（设备名 / 地址 / 项数 / 条目名与元信息），
   * 这样缩略图不只说明"这是张卡片"，还说明**卡片上都有什么、怎么排**。数据只是示意，不与本机
   * 真实设备相关；项数由组件按 `items.length` 自己算，角标数字由 `notifCount` / `deviceCount` 给。
   * 三页是因为**只有一页时分页点整列不画**：要看全分页点，缩略图就得有得翻。
   */
  const PREVIEW_PAGES: RectPage[] = $derived([
    {
      id: 'local', kind: 'local', name: t('本机'), ip: '192.168.1.23:3890',
      loading: false, error: null, dir: null, up: null,
      // 目录在前、文件在后：两种图标（文件夹 / 文件）与两档元信息（「N 项」/ 大小）都能看到
      items: [
        { id: 'preview-1-a', path: '', name: t('项目资料'), isDir: true, meta: t('{n} 项', { n: 18 }) },
        { id: 'preview-1-b', path: '', name: t('设计素材'), isDir: true, meta: t('{n} 项', { n: 7 }) },
        { id: 'preview-1-c', path: '', name: t('报告.pdf'), isDir: false, meta: '2.4 MB' },
        { id: 'preview-1-d', path: '', name: t('截图'), isDir: true, meta: t('{n} 项', { n: 32 }) },
        { id: 'preview-1-e', path: '', name: t('安装包.exe'), isDir: false, meta: '86 MB' },
      ],
    },
    {
      id: 'preview-2', kind: 'peer', name: t('书房·台式机'), ip: '192.168.1.42:3890',
      loading: false, error: null, dir: null, up: null,
      items: [
        { id: 'preview-2-a', path: '', name: t('资料'), isDir: true, meta: t('{n} 项', { n: 12 }) },
        { id: 'preview-2-b', path: '', name: t('合同.docx'), isDir: false, meta: '96 KB' },
        { id: 'preview-2-c', path: '', name: t('备份.zip'), isDir: false, meta: '640 MB' },
      ],
    },
    {
      id: 'preview-3', kind: 'peer', name: 'NAS', ip: '192.168.1.9:3890',
      loading: false, error: null, dir: null, up: null,
      items: [
        { id: 'preview-3-a', path: '', name: t('影音库'), isDir: true, meta: t('{n} 项', { n: 240 }) },
        { id: 'preview-3-b', path: '', name: t('照片'), isDir: true, meta: t('{n} 项', { n: 1280 }) },
      ],
    },
  ]);

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
  /**
   * 推送接收是否再放一层 `<发送方名>/`（后端 `AppSettings.deviceSubdir`，**默认开**，S-12）。
   *
   * 开着 = 对端推来的东西落在 `<下载目录>/<发送方名>/…` 下：两台设备推同名文件不会互相
   * 顶掉、一批东西能按来源整体看/整体清、接收需要同意的推送时那颗目录框只把"根的里面
   * 一个自己的目录"授权出去。关掉 = 和手动下载的文件落到同一层。
   */
  let deviceSubdir = $state(true);
  /**
   * 已配对设备推送是否免确认自动收（后端 `AppSettings.autoAcceptPaired`，**默认关**）。
   * 开着时直接落下载目录；关着时和未配对设备一样先点「接收」，再按「保存位置」决定落点。
   */
  let autoAcceptPaired = $state(false);
  let downloadMode = $state<'Direct' | 'AskEachTime'>('AskEachTime');
  let deviceName = $state('');
  /** 界面语言（具体语言，没有「跟随系统」）。改它 = 立刻生效并落盘（不等「保存」）。 */
  let languagePref = $state<Locale>(normalizePreference(null));
  let autoStart = $state(true);
  /**
   * 新添加的目录/文件默认开播（后端 `AppSettings.autoBroadcastNew`，默认关）。
   * 只影响之后新登记的共享项，已在列表里的开关不受影响。
   */
  let autoBroadcastNew = $state(false);
/**
   * 允许本机替别人中转（后端 `AppSettings.allowRelay` + `groupRelay`，**默认开**）。
   *
   * **一个开关管住"本机替别人做事"的两条路**：① 替其它设备盲转字节（两台设备单播不通但
   * 都连得上本机时，本机替它们搬字节，两端 TLS 端到端不变、本机看不到内容）；② 替够不着的
   * 同群成员转一份群消息（最多 3 跳）。代价都是本机的上行带宽 —— 但**首次启动就是开的**
   * （见后端 `Config::default`）：装上就能当锚点，不必先翻设置；不想出这笔开销再关掉。
   *
   * 「记住并自动复用跨网段中转」（`relayMemory`）不在这颗开关里：它不替别人做事、也不向对端
   * 暴露本机的新能力，只是本机自己的选路提示，**默认开、不给控制**（保存时一律写成开）。
   */
  let relay = $state(true);
  /**
   * 支持 LocalSend v2.2 协议（后端 `AppSettings.localsendEnabled`，**默认关**）。
   *
   * 开着且本机在线：监听 UDP 53317、收发 LocalSend 公告、提供 `/api/localsend/*` 路由，
   * 设备列表多出 LocalSend 行。离线时自动停（socket 关掉、路由 503）。LocalSend 设备
   * 不配对、不进聊天群，只走 direct 会话：文本/图片可自动接收，普通文件仍要确认。
   */
  let localsendEnabled = $state(false);
  /** LocalSend 开关的命令还没返回（防连点）。 */
  let localSendBusy = $state(false);
  const localSendStatus = $derived(getLocalSendStatus());
  const localSendView = $derived(localSendStatusView(localSendStatus));
  /** 状态行的颜色：绿=正常，琥珀=降级，红=发现没起来，灰=离线暂停。 */
  function localSendTone(tone: 'ok' | 'warn' | 'error' | 'muted'): string {
    switch (tone) {
      case 'ok':
        return 'var(--ok)';
      case 'warn':
        return 'var(--warn)';
      case 'error':
        return 'var(--danger)';
      default:
        return 'var(--muted)';
    }
  }
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
  /** 「检查更新」这颗按钮按住期间（后端在跑，事件会先把状态推过来）。 */
  let checking = $state(false);

  /**
   * 自更新那一份状态：**不是设置项** —— 它不由 `update_settings` 写，也不参与 `save()`，
   * 归后端的 `update::UpdateState` 管（事件 `update://state` 推过来）。这里只读它来画。
   */
  const update = $derived(upd.getUpdateState());
  /** 后端正在忙（检查/下载/校验/落地）：重复点没有意义，按钮按住。 */
  const updateBusy = $derived(
    update != null && ['checking', 'downloading', 'verifying', 'applying'].includes(update.phase),
  );
  /** 下载进度百分比（不知道总长时是 0，界面退化成"已下载多少"）。 */
  const updatePct = $derived(
    update?.size ? Math.min(100, Math.round((update.received / update.size) * 100)) : 0,
  );

  async function checkUpdateNow() {
    checking = true;
    try {
      await upd.checkUpdate(true);
    } catch (e: any) {
      // 检查失败是**用户主动要求的结果**，必须弹出来（启动那次静默检查失败是不弹的）。
      onToast(t('检查更新失败：{error}', { error: String(e) }));
    } finally {
      checking = false;
    }
  }

  async function installUpdateNow() {
    try {
      await upd.installUpdate();
    } catch (e: any) {
      console.error('Update install failed:', e);
      onToast(t('更新失败：{error}', { error: String(e) }));
    }
  }

  async function wipeNow() {
    wiping = true;
    try {
      const n = await wipeCredentials();
      wipeArmed = false;
      onToast(
        n > 0
          ? t('已清空 {n} 台设备的凭据，需重新配对', { n })
          : t('本机没有配对凭据'),
      );
    } catch (e: any) {
      console.error('Wipe credentials failed:', e);
      onToast(t('清空失败：{error}', { error: String(e) }));
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
        title: t('选择下载目录'),
        directory: true,
        multiple: false,
        defaultPath: downloadDir || undefined,
      });
      const dir = Array.isArray(picked) ? picked[0] : picked;
      if (dir) downloadDir = dir;
    } catch (e: any) {
      console.error('Pick directory failed:', e);
      onToast(t('打开目录选择框失败：{error}', { error: String(e) }));
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
    // 订阅一次自更新状态（重复调用没有副作用）：设置面板是唯一能看到"有新版 / 下载进度"的地方。
    void upd.watchUpdate();
    void refreshLocalSendStatus();
    loadSettings().then(() => {
      const s = getSettings();
      if (s) {
        port = s.port;
        downloadDir = s.downloadDir;
        deviceSubdir = s.deviceSubdir;
        autoAcceptPaired = s.autoAcceptPaired;
        downloadMode = s.downloadMode;
        deviceName = s.deviceName;
        languagePref = normalizePreference(s.language);
        adoptLanguagePreference(s.language);
        autoStart = s.autoStart;
        autoBroadcastNew = s.autoBroadcastNew;
        relay = s.allowRelay || s.groupRelay;
        localsendEnabled = s.localsendEnabled;
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

  async function changeLanguage(next: Locale): Promise<void> {
    const previous = languagePref;
    languagePref = next;
    // 界面先换（LocalStorage 同步写一份，下一次启动首帧就是对的），再落后端并让托盘跟字。
    setLanguagePreference(next);
    try {
      await setLanguage(next);
      await loadSettings();
      onToast(t('界面语言已切换'));
    } catch (err: any) {
      languagePref = previous;
      adoptLanguagePreference(previous);
      onToast(t('切换语言失败：{error}', { error: String(err) }));
    }
  }

  async function toggleLocalSend(next: boolean): Promise<void> {
    const previous = localsendEnabled;
    localsendEnabled = next;
    localSendBusy = true;
    try {
      const s = await setLocalSendEnabled(next);
      localsendEnabled = s.enabled;
      onToast(next ? t('LocalSend 兼容已开启') : t('LocalSend 兼容已关闭'));
    } catch (e: any) {
      // 只有写盘失败才把开关弹回去；53317 被占用/离线都在状态行里说。
      localsendEnabled = previous;
      onToast(t('切换 LocalSend 兼容失败：{error}', { error: String(e) }));
    } finally {
      localSendBusy = false;
    }
  }

  /** 状态行的「重试」：叫醒后台 guard 补一拍（不重启，在途传输不断）。 */
  async function retryLocalSend(): Promise<void> {
    localSendBusy = true;
    try {
      await retryLocalSendNow();
    } catch (e: any) {
      onToast(t('切换 LocalSend 兼容失败：{error}', { error: String(e) }));
    } finally {
      localSendBusy = false;
    }
  }

  async function save() {
    saving = true;
    try {
      await updateSettings({
        port,
        downloadDir,
        deviceSubdir,
        autoAcceptPaired,
        downloadMode,
        deviceName,
        language: languagePref,
        autoStart,
        autoBroadcastNew,
        // 一颗开关管两条路：替其它设备盲转字节 + 替群聊转消息（`allowRelay` / `groupRelay`）。
        groupRelay: relay,
        allowRelay: relay,
        // LocalSend 开关是**立即生效**的（`set_localsend_enabled`），不走这里：
        // 这里不带它 = 后端保持原值，避免保存时把刚切过的开关又写回表单里的旧状态。
        // 「记住并自动复用跨网段中转」不给控制、默认开：一律写开。
        relayMemory: true,
        transferWorkers: clampNum(transferWorkers, 1, 8, 4),
        transferBlockMb: clampNum(transferBlockMb, 1, 32, 4),
        bcExePath,
        orbShape,
      });
      onClose();
      onToast(t('设置已保存'));
    } catch (e: any) {
      console.error('Settings save failed:', e);
      onToast(t('保存失败：{error}', { error: String(e) }));
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
      deviceSubdir = s.deviceSubdir;
      autoAcceptPaired = s.autoAcceptPaired;
      downloadMode = s.downloadMode;
      deviceName = s.deviceName;
      languagePref = normalizePreference(s.language);
      adoptLanguagePreference(s.language);
      autoStart = s.autoStart;
      autoBroadcastNew = s.autoBroadcastNew;
      relay = s.allowRelay || s.groupRelay;
      // LocalSend 开关是立即生效的，**不参与取消**（`cancel` 不能把它拨回旧值；
      // 否则面板关掉前会闪一下与后端不一致的状态）。
      transferWorkers = s.transferWorkers;
      transferBlockMb = s.transferBlockMb;
      bcExePath = s.bcExePath ?? '';
      orbShape = s.orbShape;
    }
    onClose();
  }
</script>

<section class="sheet" class:open aria-label={t('设置')}>
  <header class="sheet-head">
    <span class="panel-brand" style="margin-right:auto">
      <Logo size={20} />{t('设置')}
    </span>
    <button class="icon-btn" onclick={cancel} aria-label={t('关闭设置')}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
        <path d="M6 6l12 12M18 6 6 18"/>
      </svg>
    </button>
  </header>
  <div class="sheet-body">
    <div class="field">
      <label for="language">{t('界面语言')}<Hint>
        {t('界面正文、托盘菜单与通知栏会立刻切换。首次启动使用系统语言，系统语言不在支持列表里时使用英语。')}
      </Hint></label>
      <LanguageSelect id="language" value={languagePref} ariaLabel={t('界面语言')} onChange={changeLanguage} />
    </div>
    <div class="field">
      <label for="deviceName">{t('设备名称')}</label>
      <input class="input" id="deviceName" bind:value={deviceName} />
    </div>
    <div class="field">
      <label for="downloadDir">{t('下载目录')}<Hint>
        {t('手动「下载到本地」和对端「推送」接收都默认存这里（选了「每次选择位置」时，接收需要同意的推送会先让你挑一个目录，挑的那个就是这次接收的根）；推送默认落在 发送方名 目录下（可由下面那颗开关关掉），按你选中的那些项各自的名字展开（不会和下载的文件混在一层）。')}
      </Hint></label>
      <div class="path-input">
        <input class="input" id="downloadDir" value={downloadDir} readonly />
        <button class="btn btn-secondary btn-sm" onclick={pickDownloadDir}>{t('选择')}</button>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('推送按发送方分目录')}<Hint>
        {t('打开（默认）：对端推来的东西落在 发送方名 那一层下面 —— 下载目录\\某某的电脑\\…。三件事跟着来：两台设备推同名文件不会互相顶掉；一批东西能按来源整体看、整体清；接收需要同意的推送时，那颗目录框只授权对方在根里 新建一个自己的目录，碰不到根里已经有的东西。')}<br />
        {t('关掉：推送和手动下载的文件落在同一层。同名的照旧自动改名 xxx (1).ext；代价是别人推来的 D\\… 会并进你本来就有的 下载目录\\D（已配对设备的推送对同名文件是校验通过后原子替换），「每次选择位置」下挑的那个目录也就等于直接让对方往那一层写文件名。')}
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('接收推送时放进以发送方命名的子目录')}</span>
        <label class="switch">
          <input type="checkbox" bind:checked={deviceSubdir} data-device-subdir />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('已配对设备自动接收')}<Hint>
        {t('开启：已配对设备推来的文件不再询问，直接保存到下载目录（此时下面的「保存位置」对这条不生效）。关闭（默认）：和未配对设备一样，先点「接收」，再按「保存位置」决定放到哪里。已配对设备仍必须带有效 token，凭据失效照样拒收；未配对设备始终需要你确认。')}
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('已配对设备推送免确认，直接落下载目录')}</span>
        <label class="switch">
          <input type="checkbox" bind:checked={autoAcceptPaired} data-auto-accept-paired />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label id="dlModeLabel">{t('保存位置')}</label>
      <div class="radio-group">
        <label class="radio">
          <input type="radio" name="dlMode" bind:group={downloadMode} value="Direct" />
          <span class="r-label">{t('直接保存到下载目录')}<span class="r-sub">{t('手动下载直接存；推送在你点接收后也直接存，不再问位置。')}</span></span>
        </label>
        <label class="radio">
          <input type="radio" name="dlMode" bind:group={downloadMode} value="AskEachTime" />
          <span class="r-label">{t('每次选择位置')}<span class="r-sub">{t('手动下载、以及你点接收后的推送，都先选保存位置。')}</span></span>
        </label>
      </div>
    </div>
    <div class="field">
      <label for="port">{t('服务端口')}<Hint>{t('被占用时自动 +1 探测，实际端口显示在底部状态栏。')}</Hint></label>
      <input class="input num" id="port" type="number" bind:value={port} />
    </div>
    <div class="field">
      <label for="transferWorkers">{t('传输并发数')}<Hint>
        {t('一次接收同时跑几路传输（一块 / 一个文件算一路）。局域网上 4 就够把千兆喂饱，调到 8 以上只会把连接、句柄与磁盘队列都堆起来，反而互相拖。取值 1–8，超出范围会在保存时夹住。')}
      </Hint></label>
      <input class="input num" id="transferWorkers" type="number" min="1" max="8" bind:value={transferWorkers} />
    </div>
    <div class="field">
      <label for="transferBlockMb">{t('分块大小（MB）')}<Hint>
        {t('大文件按它切片、片与片之间并行。4MB 在千兆下约 33ms 一块 —— 取消 / 失败的可感知延迟就在这个尺度，同时「一块要重传」的代价也不大。取值 1–32，超出范围会在保存时夹住；再大就不是并行，而是「一次传半个文件」。')}
      </Hint></label>
      <input class="input num" id="transferBlockMb" type="number" min="1" max="32" bind:value={transferBlockMb} />
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label id="orbShapeLabel">{t('悬浮形态')}<Hint>
        {t('收起后常驻屏幕上的那件东西长什么样。缩略图就是实物外观（角标、分页点都在），里面的设备名、地址与角标数字是示意数据。')}<br />
        {t('圆球：62px 的球，贴边会收进屏幕边缘、悬停滑回来。')}<br />
        {t('圆角矩形悬浮框：一张卡片，第一页是本机、其后每台长连接设备一页，列出各自对本机可见的共享项；贴边会收起成一条竖条（图标 + 设备名 + ip:port），悬停滑回来；直接拖动卡片里的条目可以拖到{target}里（与面板里一样），滚轮/点右侧小点换页。', { target: fileManagerLabel() })}
      </Hint></label>
      <div class="shape-picker" role="radiogroup" aria-labelledby="orbShapeLabel">
        <label class="shape-opt" title={t('圆球：贴边收起半颗，悬停滑回来')}>
          <input type="radio" name="orbShape" bind:group={orbShape} value="circle" aria-label={t('圆球')} />
          <!-- 缩略图 = **真组件 + 示例数据**（同一套 CSS/DOM，件件与实物一致），外面套一层
               缩放框（`--s` = 缩放比，`--w`/`--h` = 不含缩放的视觉外框，`--ox`/`--oy` = 组件
               原点在外框里的落点）。
               圆球那一档放大到与卡片同高（两个缩略图并排，等高才像同一层级的两个选择）。
               卡片那一档的外框**以卡片为中心**两侧各留 30px（= 右侧「间距 + 分页点列」那一条的
               宽度）：分页点收在右格里、卡片正中，不再整块偏左。
               缩略图永远画**未收起**的卡片：当前悬浮框贴在屏幕边上时，stage 上的 `.docked`
               一类样式会让真卡片只露竖条；预览里由 main.css 的预览规则逐条还原
               （设备名 / ip 在头两行，下面两列共享项）。
               整块 `inert`：缩略图不吃指针、不进 Tab 序，也不会被拖出去。
               说明文字不放在图下面（只留两张图）：两档各自是什么，看上方那颗「?」。 -->
          <span class="so-frame" style="--s:1.56; --w:72px; --h:72px" inert>
            <span class="so-scale">
              <Orb badgeCount={2} notifCount={3} expanded={false} onToggle={() => {}} onDragStart={() => {}} />
            </span>
          </span>
        </label>
        <label class="shape-opt" title={t('圆角矩形悬浮框：设备分页卡片，贴边收起成竖条，可直接拖出共享项')}>
          <input type="radio" name="orbShape" bind:group={orbShape} value="rect" aria-label={t('圆角矩形悬浮框')} />
          <span class="so-frame" style="--s:0.78; --w:236px; --h:144px; --ox:30px; --oy:5px" inert>
            <span class="so-scale">
              <RectOrb
                pages={PREVIEW_PAGES}
                deviceCount={2}
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
      <label for="bcExePath">{t('BC 可执行文件路径')}<Hint>
        {isMac()
          ? t('用于右键菜单里的「用 BeyondCompare 比较」。可以填 .app 包（如 /Applications/Beyond Compare.app），也可以填包内的 bcompare。')
          : t('用于右键菜单里的「用 BeyondCompare 比较」。例如 BCompare.exe 的完整路径。')}
      </Hint></label>
      <input
        class="input"
        id="bcExePath"
        bind:value={bcExePath}
        placeholder={isMac() ? t('留空 = 自动探测（.app 也可）') : t('留空 = 自动探测')}
      />
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('开机自启')}</label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('登录后自动启动 OrbDrop')}</span>
        <label class="switch">
          <input type="checkbox" bind:checked={autoStart} data-autostart />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('添加后自动共享')}<Hint>
        {t('打开时：新添加的目录 / 文件自动打开广播开关，当场就对长连接设备可见（与每一项自己那个开关是同一个东西，加完随时可以单独关掉）。')}<br />
        {t('默认关闭，只作用于之后添加的：已在列表里的共享项开关一个都不动 —— 手动改过的不会被这里翻回来。')}
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('新添加的目录/文件自动开启共享')}</span>
        <label class="switch">
          <input type="checkbox" bind:checked={autoBroadcastNew} data-autobroadcast />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('允许中转')}<Hint>
        {t('打开后本机替别人做事，两条路都在这一个开关里：')}<br />
        {t('① 替其它设备盲转字节：两台设备在同一张 Wi-Fi 上互相连不上时（有些 AP 会把客户端之间的流量隔开），本机连着 A 也连着 B，就替它们搬字节 —— 能直连就直连，数据端到端加密，本机只搬字节、看不到内容。不限已配对设备：开着就允许，同网段里能到达本机的任意设备都可以借（强制保留的只有"到对方给的那个地址核实目标身份"这一关，少了它本机就成了给任意局域网地址开隧道的工具）。')}<br />
        {t('② 替群聊中转消息：群里两个成员互相够不着时（跨网段、只有本机跟它们都连得上），本机替它们把消息转一份，最多 3 跳，同一份转过一次就不再转（不来回绕圈）。')}<br />
        {t('默认打开：两条路都占本机上行带宽 —— 这笔开销该不该出由你定，首次启动就是开的，不想出这笔开销就关掉；对端之间直连得了时，这个开关开不开都一样。关闭会立刻收掉本机正在替别人转的每一条。')}
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('本机替够不着彼此的其它设备搬字节、替够不着的同群成员转消息；会占用本机上行带宽')}</span>
        <label class="switch">
          <input type="checkbox" bind:checked={relay} data-relay />
          <span class="track"></span>
        </label>
      </div>
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('支持 LocalSend 协议')}<Hint>
        {t('默认关闭。打开且本机在线时，会加入 UDP 53317 组播，与局域网里的 LocalSend 设备互相发现并收发消息/文件；本机离线时自动停止，关掉后不再监听。')}<br />
        {t('LocalSend 设备不配对、不进聊天群，只走一对一；文本和图片始终自动接收并显示在会话里，普通文件仍要你确认。')}
      </Hint></label>
      <div style="display:flex; align-items:center; justify-content:space-between;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('与局域网里的 LocalSend 设备互相发现、收发消息与文件（UDP 53317）')}</span>
        <label class="switch">
          <input
            type="checkbox"
            checked={localsendEnabled}
            disabled={localSendBusy}
            data-localsend
            onchange={(e: Event) =>
              void toggleLocalSend((e.currentTarget as HTMLInputElement).checked)}
          />
          <span class="track"></span>
        </label>
      </div>
      <p style="margin:4px 0 0; font-size:var(--fs-sm); color:var(--muted);">
        {t('开启后立即生效，不等「保存设置」。53317 被占用时先回落主端口，端口空出后自动接管；不会因此自动关闭。')}
      </p>
      {#if localSendView}
        <div style="display:flex; align-items:flex-start; gap:8px; margin-top:4px;">
          <span style="font-size:var(--fs-sm); color:{localSendTone(localSendView.tone)};">
            {localSendView.text}
          </span>
          {#if localSendView.retry}
            <button
              class="btn btn-ghost btn-sm"
              style="flex:none;"
              disabled={localSendBusy}
              onclick={() => void retryLocalSend()}
            >{t('重试')}</button>
          {/if}
        </div>
      {/if}
    </div>
    <!-- 中转流量（F-106 / S-09）：替别人搬了多少 —— 只读的累计记录，不随保存写。 -->
    <div class="field">
      <label>{t('中转流量')}<Hint>
        {t('本机替别人中转累计搬过的数据量（收发两个方向之和）与连接数 —— 中转方的代价就是上行带宽，这里就是这笔账。')}<br />
        {t('只统计本机当中转方替别人搬的那部分：替其它设备盲转的字节，以及替群聊转发出去的消息。本机自己发出去、经别人中转，不在这个数里。')}<br />
        {t('连接数只数盲转的连接（消息中转不算一条连接）。它是落了盘的记录（`config.toml` 的 `relayBytesTotal` / `relayConnsTotal`），重启接着算。')}
      </Hint></label>
      {#if traffic && (traffic.bytes > 0 || traffic.connections > 0)}
        <span style="font-size:var(--fs-sm); color:var(--muted);">
          {#if traffic.connections > 0}
            {t('累计 {size} · {n} 条连接', { size: formatFileSize(traffic.bytes), n: traffic.connections })}
          {:else}
            {t('累计 {size}', { size: formatFileSize(traffic.bytes) })}
          {/if}
        </span>
      {:else}
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('还没有替别人中转 —— 有设备借本机中转之后才会有')}</span>
      {/if}
    </div>
    <!-- 版本与更新：与上面那些设置项不同，它**不走** `update_settings`、也不参与 `save()` ——
         它是"看 + 点"的一格（与下面「配对凭据」同一个性质），状态由后端推。 -->
    <div class="field">
      <label>{t('版本与更新')}<Hint>
        {t('更新包先试 GitHub、连不上或太慢（几 KB/s）自动切 Gitee，两条都是同一个包。')}<br />
        {t('装之前会校验 sha256 与开发者签名：签名对不上的一律拒装（发布仓被换包也装不进来）。')}<br />
        {t('桌面端是单文件 exe：更新时用新版本覆盖当前这个文件并自动重启，更新完依旧只有一个 exe（设置、日志、传输记录都在数据目录里，不受影响）。')}<br />
        {t('安卓端下的是 APK，装的时候系统会弹一次确认 —— 非系统应用不能静默安装。')}
      </Hint></label>
      <div style="display:flex; align-items:center; gap:8px; flex-wrap:wrap;">
        <span style="font-size:var(--fs-sm); color:var(--muted);">{t('当前 v{version}', { version: update?.current ?? '…' })}</span>
        <button
          class="btn btn-secondary btn-sm"
          onclick={checkUpdateNow}
          disabled={checking || updateBusy}
        >
          {checking ? t('检查中…') : t('检查更新')}
        </button>
        {#if update?.phase === 'available'}
          <button class="btn btn-primary btn-sm" onclick={installUpdateNow} disabled={upd.isApplying()}>
            {upd.isApplying() ? t('准备中…') : t('立即更新')}
          </button>
        {/if}
      </div>
      {#if update?.phase === 'available'}
        <span style="font-size:var(--fs-sm); color:var(--muted);">
          {#if update.size}
            {t('有新版 v{version} · {size}', { version: update.version ?? '', size: formatFileSize(update.size) })}
          {:else}
            {t('有新版 v{version}', { version: update.version ?? '' })}
          {/if}
        </span>
        <UpdateNotes
          notes={update.notes}
          empty={t('下载完会用新版本覆盖当前程序并重启。')}
        />
      {/if}
      {#if update?.phase === 'downloading' || update?.phase === 'verifying'}
        <div style="height:6px; border-radius:3px; background:var(--fg-soft); overflow:hidden;">
          <div style="height:100%; width:{updatePct}%; background:var(--accent); transition:width .15s linear;"></div>
        </div>
      {/if}
      {#if update && updateStatusText(update)}
        <span
          style="font-size:var(--fs-sm); color:{update.phase === 'error' ? 'var(--danger)' : 'var(--muted)'};"
        >
          {updateStatusText(update)}
        </span>
      {/if}
    </div>
    <!-- svelte-ignore a11y_label_has_associated_control -->
    <div class="field">
      <label>{t('配对凭据')}<Hint>
        {t('全部设备的 token 存在本机数据目录的一个文件里（仅遮挡、非加密）。这里是唯一的删除入口：清空后所有设备回到「未配对」需重新配对，已写进 Beyond Compare 的 WebDAV profile 也一并清掉。')}
      </Hint></label>
      {#if wipeArmed}
        <div style="display:flex; align-items:center; gap:8px;">
          <span style="font-size:var(--fs-sm); color:var(--muted);">{t('确认清空？所有设备需重新配对')}</span>
          <button class="btn btn-ghost btn-sm" onclick={() => (wipeArmed = false)} disabled={wiping}>{t('取消')}</button>
          <button class="btn btn-primary btn-sm" onclick={wipeNow} disabled={wiping}>
            {wiping ? t('清空中…') : t('确认清空')}
          </button>
        </div>
      {:else}
        <button class="btn btn-secondary btn-sm" onclick={() => (wipeArmed = true)}>{t('清空全部凭据')}</button>
      {/if}
    </div>
  </div>
  <footer class="sheet-foot">
    <button class="btn btn-ghost" onclick={cancel}>{t('取消')}</button>
    <button class="btn btn-primary" onclick={save} disabled={saving || localSendBusy}>
      {saving ? t('保存中…') : t('保存设置')}
    </button>
  </footer>
</section>