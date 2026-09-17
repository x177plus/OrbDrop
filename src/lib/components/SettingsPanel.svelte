<script lang="ts">
  import { onMount } from 'svelte';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { wipeCredentials } from '$lib/api';
  import { getSettings, loadSettings, updateSettings } from '$lib/stores/settings.svelte';

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
  let bcExePath = $state('');
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
        bcExePath = s.bcExePath ?? '';
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
        bcExePath,
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
      bcExePath = s.bcExePath ?? '';
    }
    onClose();
  }
</script>

<section class="sheet" class:open aria-label="设置">
  <header class="sheet-head">
    <span class="panel-brand" style="margin-right:auto">
      <span class="mark-sm"><i></i></span>设置
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
      <label for="downloadDir">下载目录</label>
      <div class="path-input">
        <input class="input" id="downloadDir" value={downloadDir} readonly />
        <button class="btn btn-secondary btn-sm" onclick={pickDownloadDir}>选择</button>
      </div>
      <p style="margin:2px 0 0; font-size:var(--fs-xs); color:var(--muted);">手动「下载到本地」和对端「推送」接收都存这里；推送会按 发送方/推送名 建子目录，不会和下载的文件混在一层。</p>
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
      <label for="port">服务端口</label>
      <input class="input num" id="port" type="number" bind:value={port} />
      <p style="margin:2px 0 0; font-size:var(--fs-xs); color:var(--muted);">被占用时自动 +1 探测，实际端口显示在底部状态栏。</p>
    </div>
    <div class="field">
      <label for="bcExePath">BC 可执行文件路径</label>
      <input class="input" id="bcExePath" bind:value={bcExePath} placeholder="留空 = 自动探测" />
      <p style="margin:2px 0 0; font-size:var(--fs-xs); color:var(--muted);">用于右键菜单里的「用 BeyondCompare 比较」，例如 BCompare.exe 的完整路径。</p>
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
      <label>配对凭据</label>
      <p style="margin:2px 0 6px; font-size:var(--fs-xs); color:var(--muted);">
        全部设备的 token 存在本机数据目录的一个文件里（仅遮挡、非加密）。这里是唯一的删除入口：
        清空后所有设备回到「未配对」需重新配对，已写进 Beyond Compare 的 WebDAV profile 也一并清掉。
      </p>
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