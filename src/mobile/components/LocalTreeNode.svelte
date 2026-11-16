<script lang="ts">
  import { untrack } from "svelte";
  import Icon from "../Icon.svelte";
  import { fileIcon } from "../util";
  import * as store from "../store.svelte";
  import * as shares from "$lib/stores/shares.svelte";
  import { formatFileSize, type TreeEntry } from "$lib/types";
  import { t } from "$lib/i18n.svelte";
  import { createLongPress } from "../longPress";
  import { fetchLocalTree, openLocalShareEntry } from "../shareTree";
  import LocalTreeNode from "./LocalTreeNode.svelte";

  // 本机共享项目录树里的一层（移动端版 `lib/components/TreeNode.svelte`）。
  //
  // 与桌面那份的差别只在手势：桌面是双击展开 / 右键菜单，手机是**点一下展开、长按菜单**
  // —— 长按即上下文菜单（与设备行同一套），不再为每一行占一颗「⋯」。
  let {
    shareId,
    entry,
    level = 1,
    /** 本节点所在目录的共享项内相对路径（共享项根 = ''），父节点逐层累加传下来。 */
    parentPath = '',
    /** 删除后由 `FilesTab` 递增：已经加载过的那一层重拉（展开状态保持不变）。 */
    refreshSignal = 0,
  }: {
    shareId: string;
    entry: TreeEntry;
    level?: number;
    parentPath?: string;
    refreshSignal?: number;
  } = $props();

  /** 本节点的共享项内相对路径（父目录路径 + 自己这一层名字）。 */
  const ownPath = $derived(parentPath ? `${parentPath}/${entry.name}` : entry.name);
  const share = $derived(shares.getShares().find((s) => s.id === shareId));

  let expanded = $state(false);
  let children = $state<TreeEntry[]>([]);
  let loaded = $state(false);
  let loading = $state(false);
  let error = $state<string | null>(null);
  /** 按住 500ms 期间的浅底：这条手势唯一可见的提示。 */
  let pressing = $state(false);

  const press = createLongPress(() => {
    pressing = false;
    store.openSheet({
      kind: 'shareEntryActions',
      shareId,
      path: ownPath,
      name: entry.name,
      isDir: entry.isDir,
      size: entry.size,
    });
  });

  async function loadChildren(): Promise<void> {
    loading = true;
    error = null;
    try {
      children = await fetchLocalTree(shareId, ownPath);
      loaded = true;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  function toggle(): void {
    if (!entry.isDir) return;
    if (!loaded && !loading) void loadChildren();
    expanded = !expanded;
  }

  async function openFile(): Promise<void> {
    const s = share;
    if (!s) {
      store.toast(t('共享项已不存在'));
      return;
    }
    try {
      await openLocalShareEntry(s, ownPath);
    } catch (e) {
      store.failed(t('打开'), e);
    }
  }

  /** 短按：目录 = 展开/收起，文件 = 用系统应用打开。 */
  function activate(): void {
    if (press.consumed()) return;
    if (entry.isDir) toggle();
    else void openFile();
  }

  function startPress(e: PointerEvent): void {
    pressing = true;
    press.start(e);
  }

  function endPress(): void {
    pressing = false;
    press.end();
  }

  // 删除动作广播（`od:local-tree-changed`）：只重拉**已经加载过**的目录，不替没展开的节点
  // 做无谓的往返；展开状态不动，用户还在看的那一层原地更新。
  $effect(() => {
    const signal = refreshSignal;
    if (signal === 0) return; // 初值：首次挂载不触发
    untrack(() => {
      if (!entry.isDir || !loaded) return;
      void loadChildren();
    });
  });

  /** 缩进按深度算（12 + depth * 20，单位 `--u`）：宽视口上整列一起放大时层级不能挤平。
   *  另外加 27u 基准：共享项根那一行前面还有一颗 16u 的展开箭头与 11u 的间距，
   *  子树要对齐的是箭头右边的文件图标，不是行首。 */
  function indent(depth: number): string {
    return `padding-left:calc(${12 + 27 + depth * 20} * var(--u))`;
  }
</script>

<div class="noderow">
  <button
    class="node"
    class:open={expanded}
    class:is-pressing={pressing}
    type="button"
    style={indent(level)}
    aria-expanded={entry.isDir ? expanded : undefined}
    onpointerdown={startPress}
    onpointermove={(e) => press.move(e)}
    onpointerup={endPress}
    onpointercancel={endPress}
    onpointerleave={endPress}
    oncontextmenu={(e) => e.preventDefault()}
    onclick={activate}
  >
    <span class="ntile" class:folder={entry.isDir}>
      <Icon name={fileIcon(entry.name, entry.isDir)} />
    </span>
    <span class="fmain">
      <span class="fname2">{entry.name}</span>
      <span class="fmeta">{entry.isDir ? '' : formatFileSize(entry.size)}</span>
    </span>
    {#if entry.isDir}
      <span class="chev" aria-hidden="true"><Icon name="chev" /></span>
    {/if}
  </button>

  {#if expanded}
    {#if loading}
      <div class="node" style={indent(level + 1)}>
        <span class="fname2" style="color:var(--muted)">{t('加载中…')}</span>
      </div>
    {:else if error}
      <div class="node" style={indent(level + 1)}>
        <span class="fname2" style="color:var(--warn)">{error}</span>
      </div>
    {:else if children.length === 0}
      <div class="node" style={indent(level + 1)}>
        <span class="fname2" style="color:var(--muted)">{t('空目录')}</span>
      </div>
    {:else}
      {#each children as child (child.name)}
        <LocalTreeNode {shareId} entry={child} level={level + 1} parentPath={ownPath} {refreshSignal} />
      {/each}
    {/if}
  {/if}
</div>