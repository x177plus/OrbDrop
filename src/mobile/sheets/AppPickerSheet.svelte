<script lang="ts">
  import type { InstalledApp } from "$lib/types";
  import { formatFileSize } from "$lib/types";
  import * as chat from "$lib/stores/chat.svelte";
  import AppIcon from "../components/AppIcon.svelte";
  import Icon from "../Icon.svelte";
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";

  /**
   * 会话里那颗「＋」→「应用」（F-119）：这张表列的是**这台设备上已安装的应用**，点一行就把
   * 它自己的安装包（`base.apk`）推给这条会话的每一台设备。
   *
   * 为什么不是借系统的"应用选择器"：安卓上没有这么一个东西 —— 分享应用要拿 `base.apk`，
   * 而拿它的路是**查包管理器**（`mobile::list_installed_apps` → `InstalledAppsPlugin.kt`），
   * 与"选文件"那两条（照片/文件选择器给 URI）不是同一回事：没有 URI、不需要授权、也不用拷
   * 任何字节。所以这张表由我们自己画。
   *
   * 名字由**后端**拼好并净化过（`fileName`），这里只显示、只交回 —— 净化判据（哪些字符会让
   * 对端整次接收判失败）在后端，与接收侧落盘那道闸门同一份（见 `transfer::manifest_for_named_paths`）。
   */
  let { onClose }: { onClose: () => void } = $props();

  const thread = $derived(chat.activeThread());

  // 进来就重读一遍：装了/卸了应用之后这张表得跟着变。旧的表**留着先画**（见 `store.loadApps`），
  // 所以重新读的这段时间里看到的是"上一次的样子 + 一个正在读的标记"，不是一片空白。
  void store.loadApps();

  const apps = $derived(store.installedApps());
  const loadState = $derived(store.appsState());

  let query = $state("");

  /**
   * 按应用名或包名筛（不区分大小写）。手机上装一百多个应用是常态，没有这一格就得靠滚动找；
   * 包名一起搜是因为**同名应用真的存在**（两个"相机"、同一个应用的不同渠道包），而包名是唯一
   * 认得出的那个。
   */
  const shown = $derived.by(() => {
    const all = apps ?? [];
    const q = query.trim().toLowerCase();
    if (!q) return all;
    return all.filter(
      (a) => a.label.toLowerCase().includes(q) || a.packageName.toLowerCase().includes(q),
    );
  });

  /**
   * 点一行 = 发送。**先关表再干活**（与「图片 / 文件」那两项同一条规矩）：值是闭包里已经抓到
   * 的那一个，关掉底板不影响；留在屏幕上只会让用户以为还要再点一次「确定」。
   */
  function send(app: InstalledApp): void {
    onClose();
    void store.sendApp(app);
  }
</script>

<Sheet
  title="发送应用"
  desc={thread
    ? `发送到「${thread.title}」：流的是它自己的安装包，落盘名叫「应用名_版本名(版本号)_包名.apk」。标着「拆包」的那些是商店拆开装的，单发安装包对端可能装不上。`
    : "发送应用到这条会话。"}
  label="选择要发送的应用"
  onClose={onClose}
>
  {#if apps === null && loadState.loading}
    <div class="empty"><Icon name="zip" /><p>正在读取应用列表…</p></div>
  {:else if apps === null && loadState.error}
    <!-- 读不到就把话说出来（原生那一跳的原文），不留一个空表让人以为"这台设备上没有应用"。 -->
    <div class="empty">
      <Icon name="zip" />
      <p>读不到应用列表</p>
      <p class="es">{loadState.error}</p>
    </div>
  {:else}
    <div class="field">
      <input class="input" type="search" placeholder="搜索应用名或包名" bind:value={query} />
    </div>
    <!-- 重读失败但旧表还在：把话说在表头上，不假装这一份就是最新的。 -->
    {#if loadState.error && apps !== null}
      <p class="field-hint">读不到最新列表（{loadState.error}），下面是上次读到的。</p>
    {/if}

    {#if (apps ?? []).length === 0}
      <div class="card list">
        <div class="empty">
          <Icon name="zip" />
          <p>没有可发送的应用</p>
          <p class="es">这台设备上没有带桌面图标的应用可发（应用列表只列得到有入口的那些）。</p>
        </div>
      </div>
    {:else}
      <div class="sec-label" style="padding-bottom:0">
        <span>应用</span>
        <!-- 重读期间（旧表还在屏幕上）说"读取中"，不留一个看不出在动的数字。 -->
        <span class="num">{loadState.loading ? "读取中…" : `${shown.length} / ${apps?.length ?? 0}`}</span>
      </div>
      {#if shown.length === 0}
        <div class="card list">
          <div class="empty"><p>没有匹配「{query.trim()}」的应用</p></div>
        </div>
      {:else}
        <div class="card list">
          {#each shown as app (app.packageName)}
            <button class="frow" type="button" onclick={() => send(app)} title={app.fileName}>
              <AppIcon pkg={app.packageName} label={app.label} />
              <span class="fmain">
                <span class="fname2">{app.label || app.packageName}</span>
                <!-- 版本名可能为空（有的应用就是不写）：那就只写版本号，不摆一个空括号。 -->
                <span class="fmeta num">
                  {app.versionName ? `${app.versionName}(${app.versionCode})` : `${app.versionCode}`} ·
                  {formatFileSize(app.size)}{app.hasSplits ? " · 拆包" : ""}
                </span>
              </span>
            </button>
          {/each}
        </div>
      {/if}
    {/if}
  {/if}
</Sheet>