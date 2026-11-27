<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import * as api from "$lib/api";
  import { formatFileSize, updateStatusText } from "$lib/types";
  import { t, setLanguagePreference, adoptLanguagePreference, normalizePreference, type Locale } from "$lib/i18n.svelte";
  import type { AppSettings, KeepAliveState, RelayTraffic } from "$lib/types";
  import * as devices from "$lib/stores/devices.svelte";
  import {
    getLocalSendStatus,
    localSendStatusView,
    refreshLocalSendStatus,
    retryLocalSendNow,
    setLocalSendEnabled,
  } from "$lib/stores/localsend.svelte";
  import * as upd from "$lib/stores/update.svelte";
  import { platform } from "$lib/stores/platform.svelte";
  import UpdateNotes from "$lib/components/UpdateNotes.svelte";
  import LanguageSelect from "$lib/components/LanguageSelect.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";

  // 后端那份设置整份留着：`update_settings` 收的是**完整** AppSettings，本页只改几个字段，
  // 其余（下载目录、autoStart、BC 路径、群聊中转…）必须原样带回去 —— 缺字段会被后端当默认值写掉。
  let saved = $state<AppSettings | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  /**
   * 自动保存（与桌面端同一口径）：**没有「保存设置」这颗按钮**。开关 / 单选改完立刻落盘；
   * 文本输入在失焦或回车时落盘；离开设置页时把仍在编辑的那一格一起提交。
   *
   * `saving` = 有一趟 `update_settings` 在飞；这期间的改动由 `saveAgain` 排队，等这一趟回来
   * 立刻再发一趟（带的是最新值），保证最后一次改动一定落盘。
   */
  let saving = $state(false);
  let saveAgain = false;
  /** 文本输入正在编辑（还没失焦）：离开页面时用它决定要不要补一次提交。 */
  let textDirty = false;
  /** 状态：只有保存失败需要露面（自动保存本身不提示）。 */
  let saveState = $state<"idle" | "saving" | "error">("idle");
  /** 界面语言（具体语言，没有「跟随系统」）：改它 = 立刻生效并落后端。 */
  let languagePref = $state<Locale>(normalizePreference(null));

  // 不在 `store.form()` 里的两项：表单只装"用户正在改的那几个"（见 store 里 resetForm 的注释），
  // 下载模式与"添加后自动共享"是本页自己的编辑态，保存时一起写回。
  let downloadMode = $state<AppSettings["downloadMode"]>("Direct");
  /** 已配对但**当前没有长连接**的设备推送是否也免确认自动收（后端
   *  `AppSettings.autoAcceptPaired`，**默认关**）。有长连接时始终自动收，不受它影响。 */
  let autoAcceptPaired = $state(false);
  /**
   * 推送接收是否再放一层 `<发送方名>/`（后端 `AppSettings.deviceSubdir`，默认开，S-12）。
   * 与桌面端同一个后端位、同一套落盘形状：开着时「文件」页那两层铺平就是"根下的散文件 +
   * 每台发送方一个目录"（见 `mobile::list_inbox`）；关掉时对端推来的东西直接落在根里。
   */
  let deviceSubdir = $state(true);
  let autoBroadcast = $state(false);
  /**
   * 「传输」两项（后端 `AppSettings.transferWorkers` / `transferBlockMb`）：与下载模式同一路数 ——
   * 不在 `store.form()` 里，是本页自己的编辑态，保存时夹进后端的取值域再写回。
   * 数字输入按字符串收（用户可以删空再填），与「监听端口」那一行同一个做法。
   */
  let transferWorkers = $state("4");
  let transferBlockMb = $state("4");

  /**
   * 「允许中转」（后端 `AppSettings.allowRelay` + `groupRelay`，**默认开**）：与桌面端**同一颗
   * 开关管两条路** —— ① 替够不着彼此的其它设备**盲转字节**（F-106 / S-09）：有些 AP 把同网段客户端
   * 之间的单播隔开，本机与 A、B 都连得上就替它们搬这一趟，两端 TLS 端到端、本机只看得到字节；
   * ② 替够不着的同群成员**转一份群消息**（F-100 / S-08，最多 3 跳）。两者都占本机上行带宽
   * （手机上是流量）—— 首次启动就是开的（后端 `Config::default`），不想出这笔开销再关掉；
   * 保存时两颗一起写。
   *
   * 手机当锚点与桌面端是**同一份实现**（`server::start_server` 两端都跑、`POST /relay/open` 的
   * 认人规则也同一条，见 `server/routes_relay.rs`），唯一的差别是"得先活着等到别人来借"——
   * 后台被系统冻结就没人借得到，见下面「后台与通知」那一格。
   */
  let relay = $state(true);

  /**
   * 支持 LocalSend v2.2 协议（后端 `AppSettings.localsendEnabled`，**默认关**）：与桌面端同一个
   * 后端位。开着且本机在线时监听 UDP 53317、与局域网里的 LocalSend 设备互相发现；离线自动停。
   */
  let localsendEnabled = $state(false);
  /** 开关命令还没返回（防连点）。 */
  let localSendBusy = $state(false);
  const localSendStatus = $derived(getLocalSendStatus());
  const localSendView = $derived(localSendStatusView(localSendStatus));
  /** 安卓只有「直接保存」一档（平台探测回来之前先按桌面画，回来后再收掉）。 */
  const isAndroid = $derived(platform().os === "android");
  // 平台探测回来后把可能残留的「每次选择位置」收回「直接保存」（例如从别的机器拷来的配置）。
  $effect(() => {
    if (isAndroid && downloadMode !== "Direct") downloadMode = "Direct";
  });
  /** 状态行的颜色档：绿=正常，琥珀=降级，红=发现没起来，灰=离线暂停（对应 `.tone-*`）。 */
  function localSendTone(tone: "ok" | "warn" | "error" | "muted"): string {
    return tone === "error" ? "danger" : tone;
  }

  /**
   * 本机替别人中转的**累计流量**（S-11，后端 `relay_traffic` 命令）：只读的用量记录 —— 它是
   * "当中转方的代价"在界面上唯一的那笔账（上行带宽 + 手机上是流量），不是设置项、不参与保存。
   *
   * 桌面端那份面板常驻 DOM、按 `open` 轮询；移动端这一页只在「设置」tab 活跃时挂着，所以进页
   * 读一次 + 5s 轮询，离开即停（`onMount` 的清理）。
   */
  let traffic = $state<RelayTraffic | null>(null);

// 「本机在线」用本地镜像而不是那份拉取结果：改它的唯一入口就是下面那颗开关，
  // 失败要能弹回来，而拉取结果从拿到那一刻起就可能过期。
  let online = $state(true);

  /**
   * 「后台与通知」那一行要显示的状态：**只有电池优化白名单能读到**（Google 官方 API），
   * ROM 的「应用启动管理 / 自启动」与「后台耗电管理」读不到 —— 所以这一行只报这一样，
   * 其余交给引导底板去说（见 `sheets/KeepAliveSheet.svelte` 与 `docs/p1-implementation.md` §22）。
   */
  let keepAlive = $state<KeepAliveState | null>(null);

  /**
   * 自更新那份状态（后端 `update::UpdateState`，事件 `update://state` 推过来）。
   *
   * 与桌面端**共用同一个 store**：状态归后端，两端只是画法不同 —— 桌面那句"立即更新"会覆盖
   * exe 并重启，手机这句要交给系统安装器。
   */
  const update = $derived(upd.getUpdateState());
  const updateBusy = $derived(
    update != null && ["checking", "downloading", "verifying", "applying"].includes(update.phase),
  );
  const updatePct = $derived(
    update?.size ? Math.min(100, Math.round((update.received / update.size) * 100)) : 0,
  );
  let checking = $state(false);

  async function checkUpdateNow(): Promise<void> {
    checking = true;
    try {
      await upd.checkUpdate(true);
    } catch (e) {
      // 用户主动点的那次失败要说出来（启动时的静默检查失败不打扰）。
      store.toast(
        t("检查更新失败：{error}", { error: e instanceof Error ? e.message : String(e) }),
        "download",
      );
    } finally {
      checking = false;
    }
  }

  async function installUpdateNow(): Promise<void> {
    try {
      await upd.installUpdate();
    } catch (e) {
      store.toast(
        t("更新失败：{error}", { error: e instanceof Error ? e.message : String(e) }),
        "download",
      );
    }
  }

  const form = $derived(store.form());

  /** 把后端返回的设置写回页面（打开页面时、自动保存回来后）。 */
  function applyServer(next: AppSettings) {
    saved = next;
    downloadMode = next.downloadMode;
    autoAcceptPaired = next.autoAcceptPaired;
    deviceSubdir = next.deviceSubdir;
    autoBroadcast = next.autoBroadcastNew;
    // 一颗开关两条后端位：任一条开着就显示成开（与桌面端读法一致）。
    relay = next.allowRelay || next.groupRelay;
    transferWorkers = String(next.transferWorkers);
    transferBlockMb = String(next.transferBlockMb);
    store.resetForm({ deviceName: next.deviceName, port: next.port });
  }

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      const s = await api.getSettings();
      languagePref = normalizePreference(s.language);
      adoptLanguagePreference(s.language);
      applyServer(s);
      // LocalSend 开关独立走 `set_localsend_enabled`，不参与这套表单：这里只读一次显示值，
      // 之后由开关自己更新（保存别的设置时不回写它）。
      localsendEnabled = s.localsendEnabled;
      textDirty = false;
      saveState = "idle";
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => {
    // 每次进设置页重灌一次：后端可能被别的窗口/事件改过（端口被占后顺延、名字被裁剪）。
    online = devices.isLocalOnline();
    // 自更新状态：订阅一次（重复调用没有副作用）。
    void upd.watchUpdate();
    void refreshLocalSendStatus();
    void load();
    // 电池优化白名单：读不到（命令没注册 / 平台没有）就保持"读取中…"，不弹错误 —— 那一行是提示，不是功能。
    void api
      .keepAliveState()
      .then((s) => {
        keepAlive = s;
      })
      .catch(() => {});

    // 中转流量（S-11）是**实时视图**：本机替别人搬的字节随时在涨，读一次只是进页那一刻的快照。
    // 它没有可订阅的事件，所以这一页开着时按 5s 轮询（与桌面端同一个节拍），离开即停。
    void refreshTraffic();
    const trafficTimer = setInterval(() => void refreshTraffic(), 5000);
    // 切到后台 / 锁屏时把文本输入里还没失焦的值提交掉：那条路不一定会触发 `change`，
    // 而输入框里的内容已经算"用户改过了"。
    const flushHidden = () => {
      if (document.visibilityState === "hidden") commitTextEdits();
    };
    document.addEventListener("visibilitychange", flushHidden);
    return () => {
      clearInterval(trafficTimer);
      document.removeEventListener("visibilitychange", flushHidden);
    };
  });

  // 离开设置页（切 tab）时把还在输入框里、尚未失焦的那一格提交掉：`change` 只保证在
  // 用户自己移开焦点时触发，程序化卸载不一定有。保存请求本身不依赖这个组件继续活着。
  // 上次自动保存失败也再试一趟，别让用户刚改的值停在未保存状态；再失败会由全局 toast 报出来。
  onDestroy(() => {
    commitTextEdits();
    if (saveState === "error") scheduleSave();
  });

  /** 读一次"本机替别人中转了多少"（只读的用量记录：不是设置项、不参与保存，失败只进控制台）。 */
  async function refreshTraffic(): Promise<void> {
    try {
      traffic = await api.relayTraffic();
    } catch (e) {
      console.error("Relay traffic load failed:", e);
    }
  }

  /**
   * 开关一 = 本机在线 / 隐身。**不是**原型那个「后台保活」：这一颗是后端真实存在的
   * `set_online`（离线 = 隐身）。**后台保活**是另一回事、另有一个入口：常驻靠前台服务
   * （F-81），而"系统会不会允许它在后台活着"只能引导用户去开 —— 见下面「后台与通知」那一格。
   */
  async function setOnline(next: boolean): Promise<boolean> {
    try {
      // 后端才是真相（离线会停广播、断链路、拒收入站），把它落回来的值拿来显示。
      online = await api.setOnline(next);
      // 把落地值报给设备 store 的镜像：那是"本机在线"唯一的另一份显示来源，分叉了就会互相打脸。
      devices.noteLocalOnline(online);
      store.toast(
        online ? t("本机已上线") : t("本机已离线 · 对端看不到你"),
        online ? "check" : undefined,
      );
      return true;
    } catch (e) {
      store.failed(t("切换在线状态"), e);
      return false;
    }
  }

  /** 开关是被浏览器先切了 DOM 再叫我们的：后端不收就得把 DOM 手动弹回去 ——
   *  `checked={online}` 的值没变，Svelte 不会再写一次它。 */
  async function toggleOnline(e: Event): Promise<void> {
    const el = e.currentTarget as HTMLInputElement;
    if (!(await setOnline(el.checked))) el.checked = online;
  }

  /** 数字输入（传输那两项）：编辑态是字符串，保存时取数字并夹进后端的取值域；空 / 非法退回上次的值。 */
  function clampNum(raw: string, lo: number, hi: number, fallback: number): number {
    const digits = raw.replace(/\D/g, "");
    if (!digits) return fallback;
    const n = Number(digits);
    return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : hi;
  }

  async function changeLanguage(next: Locale): Promise<void> {
    const previous = languagePref;
    languagePref = next;
    setLanguagePreference(next);
    try {
      const s = await api.setLanguage(next);
      saved = s;
      store.toast(t("界面语言已切换"), "check");
    } catch (err) {
      languagePref = previous;
      adoptLanguagePreference(previous);
      store.toast(
        t("切换语言失败：{error}", { error: err instanceof Error ? err.message : String(err) }),
      );
    }
  }

  async function toggleLocalSend(next: boolean): Promise<void> {
    const previous = localsendEnabled;
    localsendEnabled = next;
    localSendBusy = true;
    try {
      const s = await setLocalSendEnabled(next);
      localsendEnabled = s.enabled;
      // 保存时 `...saved` 会整份回写；把这一位同步成刚落盘的值。
      if (saved) saved = { ...saved, localsendEnabled: s.enabled };
      store.toast(
        next ? t("LocalSend 兼容已开启") : t("LocalSend 兼容已关闭"),
        "check",
      );
    } catch (e) {
      // 只有写盘失败才弹回；53317 被占用/离线都在状态行里说。
      localsendEnabled = previous;
      store.failed(t("切换 LocalSend 兼容"), e);
    } finally {
      localSendBusy = false;
    }
  }

  /** 状态行的「重试」：叫醒后台 guard 补一拍（不重启，在途传输不断）。 */
  async function retryLocalSend(): Promise<void> {
    localSendBusy = true;
    try {
      await retryLocalSendNow();
    } catch (e) {
      store.failed(t("切换 LocalSend 兼容"), e);
    } finally {
      localSendBusy = false;
    }
  }

  /**
   * 自动保存的请求体。**每次都带整份设置**（后端 `update_settings` 收的就是完整
   * `AppSettings`），没在页面里出现的字段从 `saved` 原样带回。
   */
  function buildRequest() {
    const f = form;
    if (!saved || !f) return null;
    const digits = String(f.port).replace(/\D/g, "");
    const port = digits ? Math.min(65535, Math.max(1, Number(digits))) : saved.port;
    // LocalSend 开关是**立即生效**的（`toggleLocalSend` 自己落盘）：这里显式不带它，
    // 否则一次晚到的保存会把刚切过的开关覆盖回旧值（后端 `None` = 保持原值）。
    const { localsendEnabled: _ignored, ...rest } = saved;
    return {
      ...rest,
      deviceName: f.deviceName.trim() || saved.deviceName,
      language: languagePref,
      port,
      downloadMode: isAndroid ? "Direct" : downloadMode,
      autoAcceptPaired,
      deviceSubdir,
      autoBroadcastNew: autoBroadcast,
      online,
      // 一颗开关管两条路（与桌面端同一个口径）：替其它设备盲转字节 + 替群聊转消息。
      groupRelay: relay,
      allowRelay: relay,
      transferWorkers: clampNum(transferWorkers, 1, 8, saved.transferWorkers),
      transferBlockMb: clampNum(transferBlockMb, 1, 32, saved.transferBlockMb),
    };
  }

  /** 把一次改动排进保存队列：没有在飞的请求就立刻发，否则等它回来带上最新值再发。 */
  function scheduleSave(): void {
    if (!saved) return;
    saveAgain = true;
    if (!saving) void runSave();
  }

  /**
   * 自动保存循环：一趟只发一个 `update_settings`；这一趟期间又有改动（`saveAgain`）就再发
   * 一趟。后端返回的是**落盘后的真值**（取值被夹住、语言被规范化），没有新改动、也没有正在
   * 编辑的文本时才写回页面；有改动时保留用户刚输入的那份，下一趟一起带上。
   *
   * 失败不自动重试（写盘失败 / 后端忙时重试只会刷屏）：状态条变成「保存失败」，点它重试；
   * 用户继续改设置也会照常触发下一趟。
   */
  async function runSave(): Promise<void> {
    if (!saved || saving) {
      saveAgain = true;
      return;
    }
    saving = true;
    saveState = "saving";
    let failed = false;
    try {
      while (saveAgain) {
        saveAgain = false;
        const req = buildRequest();
        if (!req) break;
        const next = await api.updateSettings(req);
        if (!saveAgain && !textDirty) {
          applyServer(next);
        } else {
          saved = next;
        }
      }
      saveState = "idle";
    } catch (e) {
      failed = true;
      saveState = "error";
      store.failed(t("保存设置"), e);
    } finally {
      saving = false;
      // 失败时保留 `saveAgain`：状态条上的「重试」会再发一趟。
      if (!failed && saveAgain) void runSave();
    }
  }

  /** 文本输入提交：失焦 / 回车 / 离开设置页都走这里。 */
  function commitTextEdits(): void {
    if (!textDirty) return;
    textDirty = false;
    scheduleSave();
  }
</script>

{#if loading && !saved}
  <div class="body">
    <div class="card list">
      <div class="empty">
        <Icon name="settings" />
        <p>{t("正在读取设置")}</p>
        <p class="es">{t("配置都在本机后端，读回来就填上。")}</p>
      </div>
    </div>
  </div>
{:else if !saved}
  <div class="body">
    <div class="card list">
      <div class="empty">
        <Icon name="settings" />
        <p>{t("读取设置失败")}</p>
        <p class="es">{loadError}</p>
        <button class="btn btn-secondary" type="button" onclick={() => void load()}>{t("重试")}</button>
      </div>
    </div>
  </div>
{:else}
  <div class="body">
    {#if saveState === "error"}
      <div class="autosave-bar is-error" role="status">
        <span>{t("保存失败")}</span>
        <button class="btn btn-secondary btn-sm" type="button" onclick={scheduleSave}>{t("重试")}</button>
      </div>
    {/if}
    <div class="sec">
      <div class="sec-label"><span>{t("语言")}</span></div>
      <div class="card gcard lang-card">
        <div class="srow">
          <span class="slabel">{t("界面语言")}</span>
          <LanguageSelect value={languagePref} ariaLabel={t("界面语言")} onChange={changeLanguage} />
          <p class="shelp">
            {t("切换后立即生效。")}
          </p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("本机")}</span></div>
      <div class="card gcard">
        <div class="srow">
          <span class="slabel">{t("本机名称")}</span>
          <input
            class="input"
            maxlength="20"
            aria-label={t("本机名称")}
            value={form?.deviceName ?? ""}
            oninput={(e: Event) => {
              store.editForm({ deviceName: (e.currentTarget as HTMLInputElement).value });
              textDirty = true;
            }}
            onchange={commitTextEdits}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
            }}
          />
        </div>
        <div class="srow">
          <span class="slabel">{t("监听端口")}</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label={t("监听端口")}
            value={form?.port ?? ""}
            oninput={(e: Event) => {
              store.editForm({ port: (e.currentTarget as HTMLInputElement).value });
              textDirty = true;
            }}
            onchange={commitTextEdits}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
            }}
          />
          <p class="shelp">{t("默认 3890；被占用时自动探测 3891–3899。")}</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("文件")}</span></div>
      <div class="card gcard">
        <!-- 原型这行是「收件目录」：移动端的收件目录由后端定、不落配置，没有可编辑也没有可回填的值。
             唯一的落盘目录就是下载目录（手动下载与推送接收都写它）。 -->
        <div class="srow">
          <span class="slabel">{t("下载目录")}</span>
          <input class="input" value={saved.downloadDir} readonly aria-label={t("下载目录")} />
        </div>
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("推送按发送方分目录")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={deviceSubdir}
                aria-label={t("推送按发送方分目录")}
                onchange={(e: Event) => {
                  deviceSubdir = (e.currentTarget as HTMLInputElement).checked;
                  scheduleSave();
                }}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">
            {t("开启后按发送方分目录存放推送。")}
          </p>
        </div>
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("已配对设备自动接收")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={autoAcceptPaired}
                aria-label={t("已配对设备自动接收")}
                onchange={(e: Event) => {
                  autoAcceptPaired = (e.currentTarget as HTMLInputElement).checked;
                  scheduleSave();
                }}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">
            {t("已配对但没有长连接的设备也免确认直接接收。")}
          </p>
        </div>
        <div class="srow">
          <span class="slabel">{t("保存位置")}</span>
          <!-- 取值是后端的 `DownloadMode`（`Direct` / `AskEachTime`），原型里那两个小写字面量是 mock。 -->
          <label class="radio" class:is-checked={downloadMode === "Direct"}>
            <input
              type="radio"
              name="dlMode"
              checked={downloadMode === "Direct"}
              onchange={() => {
                downloadMode = "Direct";
                scheduleSave();
              }}
            />
            <span class="rmain">
              <span class="rname">{t("直接保存到下载目录")}</span>
              <span class="rdesc">{t("手动下载直接存；接收推送也直接存到下载目录。")}</span>
            </span>
          </label>
          {#if !isAndroid}
          <label class="radio" class:is-checked={downloadMode === "AskEachTime"}>
            <input
              type="radio"
              name="dlMode"
              checked={downloadMode === "AskEachTime"}
              onchange={() => {
                downloadMode = "AskEachTime";
                scheduleSave();
              }}
            />
            <span class="rmain">
              <span class="rname">{t("每次选择位置")}</span>
              <span class="rdesc">{t("手动下载前选择保存位置；接收推送仍保存到下载目录（手机没有可写的目录选择器）。")}</span>
            </span>
          </label>
          {:else}
            <p class="shelp">
              {t("安卓上固定保存到下载目录。")}
            </p>
          {/if}
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("传输")}</span></div>
      <div class="card gcard">
        <div class="srow">
          <span class="slabel">{t("传输并发数")}</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label={t("传输并发数")}
            value={transferWorkers}
            oninput={(e: Event) => {
              transferWorkers = (e.currentTarget as HTMLInputElement).value;
              textDirty = true;
            }}
            onchange={commitTextEdits}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
            }}
          />
          <p class="shelp">{t("取值 1–8，默认 4。")}</p>
        </div>
        <div class="srow">
          <span class="slabel">{t("分块大小（MB）")}</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label={t("分块大小（MB）")}
            value={transferBlockMb}
            oninput={(e: Event) => {
              transferBlockMb = (e.currentTarget as HTMLInputElement).value;
              textDirty = true;
            }}
            onchange={commitTextEdits}
            onkeydown={(e: KeyboardEvent) => {
              if (e.key === "Enter") (e.currentTarget as HTMLInputElement).blur();
            }}
          />
          <p class="shelp">{t("取值 1–32，默认 4。")}</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("中转")}</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("允许中转")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={relay}
                aria-label={t("允许中转")}
                onchange={(e: Event) => {
                  relay = (e.currentTarget as HTMLInputElement).checked;
                  scheduleSave();
                }}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">
            {t("替够不着彼此的设备和同群成员转发数据，会占用本机上行带宽。")}
          </p>
        </div>
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("支持 LocalSend 协议")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={localsendEnabled}
                disabled={localSendBusy}
                data-localsend
                aria-label={t("支持 LocalSend 协议")}
                onchange={(e: Event) =>
                  void toggleLocalSend((e.currentTarget as HTMLInputElement).checked)}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">
            {t("与局域网里的 LocalSend 设备互相发现、互传文件；设备不配对，普通文件仍需确认。")}
          </p>
          {#if localSendView}
            <div class="shelp-row">
              <p class="shelp tone-{localSendTone(localSendView.tone)}">
                {localSendView.text}
              </p>
              {#if localSendView.retry}
                <button
                  class="btn btn-secondary btn-sm"
                  type="button"
                  disabled={localSendBusy}
                  onclick={() => void retryLocalSend()}
                >{t("重试")}</button>
              {/if}
            </div>
          {/if}
        </div>
        <div class="srow">
          <span class="slabel">{t("中转流量")}</span>
          {#if traffic && (traffic.bytes > 0 || traffic.connections > 0)}
            <p class="shelp">
              {t(
                "本机替别人中转累计 {size}{conn}（只算本机当中转方替别人搬的那部分，重启接着算）。",
                {
                  size: formatFileSize(traffic.bytes),
                  conn:
                    traffic.connections > 0
                      ? " · " + t("{n} 条连接", { n: traffic.connections })
                      : "",
                },
              )}
            </p>
          {:else}
            <p class="shelp">{t("还没有中转流量。")}</p>
          {/if}
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("运行")}</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("本机在线")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={online}
                aria-label={t("本机在线")}
                onchange={(e: Event) => void toggleOnline(e)}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">{t("关闭后对局域网隐身。")}</p>
        </div>
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("添加后自动共享")}</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={autoBroadcast}
                aria-label={t("添加后自动共享")}
                onchange={(e: Event) => {
                  autoBroadcast = (e.currentTarget as HTMLInputElement).checked;
                  scheduleSave();
                }}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">{t("只影响之后新添加的共享项；已经在列表里的开关一个都不动。")}</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("后台与通知")}</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("保活与通知")}</span>
            <button
              class="btn btn-secondary btn-sm"
              type="button"
              onclick={() => store.openSheet({ kind: "keepAlive" })}
            >
              {t("检查")}
            </button>
          </div>
          <p class="shelp">
            {t(
              "电池优化：{status}。留在默认档时系统会在后台冻结应用。",
              {
                status:
                  keepAlive === null
                    ? t("读取中…")
                    : keepAlive.batteryOptimizationIgnored
                      ? t("已允许")
                      : t("未允许"),
              },
            )}
          </p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>{t("版本与更新")}</span></div>
      <!-- 这一格是设置页的最后一格。原型末尾还有一张「OrbDrop 移动端 / 需求 v0.5…」的卡片，
           而且这一行底下压着一句"更新包先试 GitHub…"的小字 —— 两张都**不搬**：版本号这里本来
           就有，卡片只是把同一件事说第二遍，小字则在出错时和真正的报错抢视线。 -->
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">{t("当前版本 v{version}", { version: update?.current ?? "…" })}</span>
            <button
              class="btn btn-secondary btn-sm"
              type="button"
              disabled={checking || updateBusy}
              onclick={() => void checkUpdateNow()}
            >
              {checking ? t("检查中…") : t("检查更新")}
            </button>
          </div>
        </div>
        {#if update?.phase === "available"}
          <div class="srow">
            <div class="srow-top">
              <span class="slabel">v{update.version}{update.size ? ` · ${formatFileSize(update.size)}` : ""}</span>
              <button
                class="btn btn-primary btn-sm"
                type="button"
                disabled={upd.isApplying()}
                onclick={() => void installUpdateNow()}
              >
                {upd.isApplying() ? t("准备中…") : t("下载并安装")}
              </button>
            </div>
            <UpdateNotes
              notes={update.notes}
              empty={t("下载完会打开系统安装器，需要你确认一次安装（非系统应用不能静默安装）。")}
            />
          </div>
        {/if}
        {#if update?.phase === "downloading" || update?.phase === "verifying"}
          <div class="srow">
            <div class="pbar"><i style="width:{updatePct}%"></i></div>
          </div>
        {/if}
        {#if update && updateStatusText(update)}
          <div class="srow">
            <p class="shelp" class:tone-danger={update.phase === "error"}>
              {updateStatusText(update)}
            </p>
          </div>
        {/if}
      </div>
    </div>
  </div>
{/if}