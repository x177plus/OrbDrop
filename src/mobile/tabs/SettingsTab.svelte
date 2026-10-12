<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "$lib/api";
  import { formatFileSize, updateStatusText } from "$lib/types";
  import type { AppSettings, KeepAliveState, RelayTraffic } from "$lib/types";
  import * as devices from "$lib/stores/devices.svelte";
  import * as upd from "$lib/stores/update.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";

  // 后端那份设置整份留着：`update_settings` 收的是**完整** AppSettings，本页只改几个字段，
  // 其余（下载目录、autoStart、BC 路径、群聊中转…）必须原样带回去 —— 缺字段会被后端当默认值写掉。
  let saved = $state<AppSettings | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let saving = $state(false);

  // 不在 `store.form()` 里的两项：表单只装"用户正在改的那几个"（见 store 里 resetForm 的注释），
  // 下载模式与"添加后自动共享"是本页自己的编辑态，保存时一起写回。
  let downloadMode = $state<AppSettings["downloadMode"]>("Direct");
  let autoBroadcast = $state(false);
  /**
   * 「传输」两项（后端 `AppSettings.transferWorkers` / `transferBlockMb`）：与下载模式同一路数 ——
   * 不在 `store.form()` 里，是本页自己的编辑态，保存时夹进后端的取值域再写回。
   * 数字输入按字符串收（用户可以删空再填），与「监听端口」那一行同一个做法。
   */
  let transferWorkers = $state("4");
  let transferBlockMb = $state("4");

  /**
   * 「允许中转」（后端 `AppSettings.allowRelay` + `groupRelay`，默认关）：与桌面端**同一颗开关
   * 管两条路** —— ① 替够不着彼此的其它设备**盲转字节**（F-106 / S-09）：有些 AP 把同网段客户端
   * 之间的单播隔开，本机与 A、B 都连得上就替它们搬这一趟，两端 TLS 端到端、本机只看得到字节；
   * ② 替够不着的同群成员**转一份群消息**（F-100 / S-08，最多 3 跳）。两者都占本机上行带宽
   * （手机上是流量），所以默认关，保存时两颗一起写。
   *
   * 手机当锚点与桌面端是**同一份实现**（`server::start_server` 两端都跑、`POST /relay/open` 的
   * 认人规则也同一条，见 `server/routes_relay.rs`），唯一的差别是"得先活着等到别人来借"——
   * 后台被系统冻结就没人借得到，见下面「后台与通知」那一格。
   */
  let relay = $state(false);

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
   * ROM 的「应用启动管理」那三个开关读不到 —— 所以这一行只报这一样，其余交给引导底板去说
   * （见 `sheets/KeepAliveSheet.svelte` 与 `docs/p1-implementation.md` §22）。
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
      store.toast(`检查更新失败：${e instanceof Error ? e.message : String(e)}`, "download");
    } finally {
      checking = false;
    }
  }

  async function installUpdateNow(): Promise<void> {
    try {
      await upd.installUpdate();
    } catch (e) {
      store.toast(`更新失败：${e instanceof Error ? e.message : String(e)}`, "download");
    }
  }

  const form = $derived(store.form());

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      const s = await api.getSettings();
      saved = s;
      downloadMode = s.downloadMode;
      autoBroadcast = s.autoBroadcastNew;
      transferWorkers = String(s.transferWorkers);
      transferBlockMb = String(s.transferBlockMb);
      // 一颗开关两条后端位：任一条开着就显示成开（与桌面端读法一致）。
      relay = s.allowRelay || s.groupRelay;
      store.resetForm({ deviceName: s.deviceName, port: s.port });
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
    return () => clearInterval(trafficTimer);
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
      store.toast(online ? "本机已上线" : "本机已离线 · 对端看不到你", online ? "check" : undefined);
      return true;
    } catch (e) {
      store.failed("切换在线状态", e);
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

  async function save(): Promise<void> {
    const f = form;
    if (!saved || !f || saving) return;
    saving = true;
    try {
      const digits = String(f.port).replace(/\D/g, "");
      const next = await api.updateSettings({
        ...saved,
        deviceName: f.deviceName.trim() || saved.deviceName,
        port: digits ? Number(digits) : saved.port,
        downloadMode,
        autoBroadcastNew: autoBroadcast,
        online,
        // 一颗开关管两条路（与桌面端同一个口径）：替其它设备盲转字节 + 替群聊转消息。
        groupRelay: relay,
        allowRelay: relay,
        transferWorkers: clampNum(transferWorkers, 1, 8, saved.transferWorkers),
        transferBlockMb: clampNum(transferBlockMb, 1, 32, saved.transferBlockMb),
      });
      // 后端会规范化（端口被占时顺延、名字裁剪），保存后再读一次让表单跟着真值走。
      saved = next;
      downloadMode = next.downloadMode;
      autoBroadcast = next.autoBroadcastNew;
      relay = next.allowRelay || next.groupRelay;
      transferWorkers = String(next.transferWorkers);
      transferBlockMb = String(next.transferBlockMb);
      store.resetForm({ deviceName: next.deviceName, port: next.port });
      store.toast("设置已保存", "check");
    } catch (e) {
      store.failed("保存设置", e);
    } finally {
      saving = false;
    }
  }
</script>

{#if loading && !saved}
  <div class="body">
    <div class="card list">
      <div class="empty">
        <Icon name="settings" />
        <p>正在读取设置</p>
        <p class="es">配置都在本机后端，读回来就填上。</p>
      </div>
    </div>
  </div>
{:else if !saved}
  <div class="body">
    <div class="card list">
      <div class="empty">
        <Icon name="settings" />
        <p>读取设置失败</p>
        <p class="es">{loadError}</p>
        <button class="btn btn-secondary" type="button" onclick={() => void load()}>重试</button>
      </div>
    </div>
  </div>
{:else}
  <div class="body">
    <div class="sec">
      <div class="sec-label"><span>本机</span></div>
      <div class="card gcard">
        <div class="srow">
          <span class="slabel">本机名称</span>
          <input
            class="input"
            maxlength="20"
            aria-label="本机名称"
            value={form?.deviceName ?? ""}
            oninput={(e: Event) => store.editForm({ deviceName: (e.currentTarget as HTMLInputElement).value })}
          />
        </div>
        <div class="srow">
          <span class="slabel">监听端口</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label="监听端口"
            value={form?.port ?? ""}
            oninput={(e: Event) => store.editForm({ port: (e.currentTarget as HTMLInputElement).value })}
          />
          <p class="shelp">默认 3890；被占用时自动探测 3891–3899。</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>文件</span></div>
      <div class="card gcard">
        <!-- 原型这行是「收件目录」：移动端的收件目录由后端定、不落配置，没有可编辑也没有可回填的值。
             唯一的落盘目录就是下载目录（手动下载与推送接收都写它）。 -->
        <div class="srow">
          <span class="slabel">下载目录</span>
          <input class="input" value={saved.downloadDir} readonly aria-label="下载目录" />
        </div>
        <div class="srow">
          <span class="slabel">下载模式</span>
          <!-- 取值是后端的 `DownloadMode`（`Direct` / `AskEachTime`），原型里那两个小写字面量是 mock。 -->
          <label class="radio" class:is-checked={downloadMode === "Direct"}>
            <input type="radio" name="dlMode" value="Direct" bind:group={downloadMode} />
            <span class="rmain">
              <span class="rname">直接存下载目录</span>
              <span class="rdesc">引用下载时不再询问位置。</span>
            </span>
          </label>
          <label class="radio" class:is-checked={downloadMode === "AskEachTime"}>
            <input type="radio" name="dlMode" value="AskEachTime" bind:group={downloadMode} />
            <span class="rmain">
              <span class="rname">每次弹窗选位置</span>
              <span class="rdesc">每次下载前选择保存位置。</span>
            </span>
          </label>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>传输</span></div>
      <div class="card gcard">
        <div class="srow">
          <span class="slabel">传输并发数</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label="传输并发数"
            value={transferWorkers}
            oninput={(e: Event) => (transferWorkers = (e.currentTarget as HTMLInputElement).value)}
          />
          <p class="shelp">一次接收同时跑几路传输；取值 1–8，默认 4，超出范围保存时自动夹住。</p>
        </div>
        <div class="srow">
          <span class="slabel">分块大小（MB）</span>
          <input
            class="input num"
            inputmode="numeric"
            aria-label="分块大小（MB）"
            value={transferBlockMb}
            oninput={(e: Event) => (transferBlockMb = (e.currentTarget as HTMLInputElement).value)}
          />
          <p class="shelp">大文件按它切片、片与片之间并行；取值 1–32，默认 4，超出范围保存时自动夹住。</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>中转</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">允许中转</span>
            <label class="switch">
              <input type="checkbox" bind:checked={relay} aria-label="允许中转" />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">
            替够不着彼此的其它设备搬字节（有些 Wi‑Fi 把同网段客户端之间的单播隔开），也替够不着的
            同群成员转消息；不限已配对设备。会占用本机上行带宽（手机上是流量），关掉立刻收掉正在
            替别人转的每一条。
          </p>
        </div>
        <div class="srow">
          <span class="slabel">中转流量</span>
          {#if traffic && (traffic.bytes > 0 || traffic.connections > 0)}
            <p class="shelp">
              本机替别人中转累计 {formatFileSize(traffic.bytes)}{traffic.connections > 0
                ? ` · ${traffic.connections} 条连接`
                : ""}（只算本机当中转方替别人搬的那部分，重启接着算）。
            </p>
          {:else}
            <p class="shelp">还没有替别人中转 —— 有设备借本机中转之后才会有这个数。</p>
          {/if}
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>运行</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">本机在线</span>
            <label class="switch">
              <input
                type="checkbox"
                checked={online}
                aria-label="本机在线"
                onchange={(e: Event) => void toggleOnline(e)}
              />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">关掉＝隐身：停止广播、断开全部长连接，也收不到别人的推送。</p>
        </div>
        <div class="srow">
          <div class="srow-top">
            <span class="slabel">添加后自动共享</span>
            <label class="switch">
              <input type="checkbox" bind:checked={autoBroadcast} aria-label="添加后自动共享" />
              <span class="track"></span>
            </label>
          </div>
          <p class="shelp">只影响之后新添加的共享项；已经在列表里的开关一个都不动。</p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>后台与通知</span></div>
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top" style="justify-content:space-between;">
            <span class="slabel">保活与通知</span>
            <button
              class="btn btn-secondary btn-sm"
              type="button"
              onclick={() => store.openSheet({ kind: "keepAlive" })}
            >
              检查
            </button>
          </div>
          <p class="shelp">
            电池优化：{keepAlive === null
              ? "读取中…"
              : keepAlive.batteryOptimizationIgnored
                ? "已允许"
                : "未允许"}。系统那一层（自启动 / 后台活动）与通知横幅都在这里检查 ——
            留在默认档时，应用在后台会被系统冻结：通知栏那条「OrbDrop 正在运行」还挂着，
            对端却已经看不到你了。
          </p>
        </div>
      </div>
    </div>

    <div class="sec">
      <div class="sec-label"><span>版本与更新</span></div>
      <!-- 这一格是设置页的最后一格。原型末尾还有一张「OrbDrop 移动端 / 需求 v0.5…」的卡片，
           而且这一行底下压着一句"更新包先试 GitHub…"的小字 —— 两张都**不搬**：版本号这里本来
           就有，卡片只是把同一件事说第二遍，小字则在出错时和真正的报错抢视线。 -->
      <div class="card gcard">
        <div class="srow">
          <div class="srow-top" style="justify-content:space-between;">
            <span class="slabel">当前版本 v{update?.current ?? "…"}</span>
            <button
              class="btn btn-secondary btn-sm"
              type="button"
              disabled={checking || updateBusy}
              onclick={() => void checkUpdateNow()}
            >
              {checking ? "检查中…" : "检查更新"}
            </button>
          </div>
        </div>
        {#if update?.phase === "available"}
          <div class="srow">
            <div class="srow-top" style="justify-content:space-between;">
              <span class="slabel">v{update.version}{update.size ? ` · ${formatFileSize(update.size)}` : ""}</span>
              <button
                class="btn btn-primary btn-sm"
                type="button"
                disabled={upd.isApplying()}
                onclick={() => void installUpdateNow()}
              >
                {upd.isApplying() ? "准备中…" : "下载并安装"}
              </button>
            </div>
            <p class="shelp">
              {update.notes ?? "下载完会打开系统安装器，需要你确认一次安装（非系统应用不能静默安装）。"}
            </p>
          </div>
        {/if}
        {#if update?.phase === "downloading" || update?.phase === "verifying"}
          <div class="srow">
            <div style="height:calc(6 * var(--u)); border-radius:calc(3 * var(--u)); background:var(--fg-soft); overflow:hidden;">
              <div
                style="height:100%; width:{updatePct}%; background:var(--accent); transition:width .15s linear;"
              ></div>
            </div>
          </div>
        {/if}
        {#if update && updateStatusText(update)}
          <div class="srow">
            <p class="shelp" style="color:{update.phase === "error" ? "var(--danger)" : "var(--muted)"};">
              {updateStatusText(update)}
            </p>
          </div>
        {/if}
      </div>
    </div>

    <div class="sec">
      <!-- 原型末尾还有一颗「清空全部凭据」（`wipe_credentials`）：危险操作，移动端本阶段不做。 -->
      <button class="btn btn-primary btn-block" type="button" disabled={saving} onclick={() => void save()}>
        保存设置
      </button>
    </div>
  </div>
{/if}