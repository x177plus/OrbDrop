<script lang="ts">
  import { onMount } from 'svelte';
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
  let inboxDir = $state('');
  let deviceName = $state('');
  let autoStart = $state(true);
  let saving = $state(false);

  onMount(() => {
    loadSettings().then(() => {
      const s = getSettings();
      if (s) {
        port = s.port;
        downloadDir = s.downloadDir;
        downloadMode = s.downloadMode;
        inboxDir = s.inboxDir;
        deviceName = s.deviceName;
        autoStart = s.autoStart;
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
        inboxDir,
        deviceName,
        autoStart,
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
      inboxDir = s.inboxDir;
      deviceName = s.deviceName;
      autoStart = s.autoStart;
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
        <button class="btn btn-secondary btn-sm" onclick={() => onToast('请通过系统文件对话框选择目录')}>选择</button>
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
          <span class="r-label">每次询问<span class="r-sub">收到文件时先征求确认</span></span>
        </label>
      </div>
    </div>
    <div class="field">
      <label for="inboxDir">收件目录</label>
      <div class="path-input">
        <input class="input" id="inboxDir" value={inboxDir} readonly />
        <button class="btn btn-secondary btn-sm" onclick={() => onToast('请通过系统文件对话框选择目录')}>选择</button>
      </div>
    </div>
    <div class="field">
      <label for="port">服务端口</label>
      <input class="input num" id="port" type="number" bind:value={port} />
      <p style="margin:2px 0 0; font-size:var(--fs-xs); color:var(--muted);">被占用时自动 +1 探测，实际端口显示在底部状态栏。</p>
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
  </div>
  <footer class="sheet-foot">
    <button class="btn btn-ghost" onclick={cancel}>取消</button>
    <button class="btn btn-primary" onclick={save} disabled={saving}>
      {saving ? '保存中…' : '保存设置'}
    </button>
  </footer>
</section>