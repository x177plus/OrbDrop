<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "$lib/api";
  import type { AppSettings } from "$lib/types";
  import * as devices from "$lib/stores/devices.svelte";
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

  // 「本机在线」用本地镜像而不是那份拉取结果：改它的唯一入口就是下面那颗开关，
  // 失败要能弹回来，而拉取结果从拿到那一刻起就可能过期。
  let online = $state(true);

  const form = $derived(store.form());

  async function load(): Promise<void> {
    loading = true;
    loadError = null;
    try {
      const s = await api.getSettings();
      saved = s;
      downloadMode = s.downloadMode;
      autoBroadcast = s.autoBroadcastNew;
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
    void load();
  });

  /**
   * 开关一 = 本机在线 / 隐身。**不是**原型那个「后台保活」：Android 上的常驻靠前台服务
   * （F-81，本阶段不做），而这里这颗开关是后端真实存在的 `set_online`。
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
      });
      // 后端会规范化（端口被占时顺延、名字裁剪），保存后再读一次让表单跟着真值走。
      saved = next;
      downloadMode = next.downloadMode;
      autoBroadcast = next.autoBroadcastNew;
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
      <div class="card gcard">
        <div class="about">
          <span class="an">OrbDrop 移动端</span>
          <span class="av2">需求 v0.5 · 局域网文件中转工具 · 仅局域网内使用</span>
        </div>
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