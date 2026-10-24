<script lang="ts">
  import { imageDataUrl } from '$lib/images';
  import { onDestroy, type Snippet } from 'svelte';

  /**
   * 会话里一张图片的缩略图（桌面端与移动端共用）。
   *
   * 读不出来（文件太大 / 已经不在盘上 / 不是图片）就渲染调用方给的 `fallback` —— 那条气泡
   * 退回原来的"文件图标 + 名字"样式，用户至少还点得开「用系统程序打开」。所以**加载中也是
   * 画 fallback**：闪一下文件行总比留一块空白好。
   *
   * 取数由这里的 `$effect` 驱动（不是渲染期读缓存）：缓存有上限，淘汰之后若还在渲染期读，
   * 就会"读 → 淘汰 → 再读"转成停不下来的循环，见 `lib/images.ts` 的文件头。
   */
  let {
    path,
    label = '图片',
    onopen,
    onlayout,
    flush = false,
    children,
  }: {
    /** 本机上那份图片的绝对路径。 */
    path: string;
    /** 无障碍名称 / 悬停提示（一般是这条文件消息的名字）。 */
    label?: string;
    /** 点缩略图（看大图）。没给就只是个不可点的预览。 */
    onopen?: () => void;
    /** 缩略图真正挂上（`<img>` 的 `load`）时通知宿主：图片高度此刻才定下来，会话那边据此重新吸底。 */
    onlayout?: () => void;
    /**
     * 铺满宿主（会话气泡）：去掉缩略图自己的边框、底色与圆角，图片贴齐容器边缘，
     * 圆角跟着宿主走。宿主要相应地把自身内边距让出来（见 `main.css` / `mobile.css`
     * 的 `.chat-bubble-media` / `.bubble.media`）。
     */
    flush?: boolean;
    /** 读不出来时画它（调用方给的是那条气泡原来的"文件图标 + 名字"样式）。 */
    children?: Snippet;
  } = $props();

  let url = $state('');
  /**
   * 上一次认领的路径（**非响应式**：只用来挡住 `path` 没变时的重跑）。
   *
   * 调用处写的是 `path={item.path}`，Svelte 编译成 `get path() { return item.path }` ——
   * 会话每次快照都重建 item 对象，这个 getter 的依赖是 **item 本身**，不是路径字符串。
   * 于是每条新消息都会让这个 effect 重跑一次。以前它无条件 `url = ''`：缩略图闪回占位行，
   * 而会话的吸底恰好量在那一瞬，图片回来后高度差就成了“差半行”。路径没变就别动。
   */
  let shownPath: string | null = null;
  /** 取数代号：换到另一条图片时旧回包作废。 */
  let request = 0;
  let alive = true;
  onDestroy(() => {
    alive = false;
  });

  $effect(() => {
    const target = path;
    if (target === shownPath) return;
    shownPath = target;
    url = '';
    if (!target) return;
    const token = ++request;
    void imageDataUrl(target)
      .then((next) => {
        if (alive && token === request) url = next;
      })
      .catch(() => {
        // 读不出来就走 fallback：那句话（"太大，不在会话里预览"之类）在气泡上没有地方写，
        // 点了「用系统程序打开」自然就看到了。
      });
  });
</script>

{#if url}
  {#if onopen}
    <button
      class="ip-thumb"
      class:flush
      type="button"
      title="点一下看大图"
      aria-label="看大图：{label}"
      onclick={onopen}
    >
      <img src={url} alt={label} draggable="false" onload={onlayout} />
    </button>
  {:else}
    <span class="ip-thumb static" class:flush>
      <img src={url} alt={label} draggable="false" onload={onlayout} />
    </span>
  {/if}
{:else}
  {@render children?.()}
{/if}

<style>
  /* 只用基础色板里的变量（不写 `color-mix()` / `oklch()`）：这份样式会进产物那份 CSS，
     而移动端产物要过 Chrome 90 的兼容门禁，见 `scripts/check-compat.mjs`。 */
  .ip-thumb {
    display: block;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--fg-soft);
    overflow: hidden;
    cursor: zoom-in;
    line-height: 0;
    max-width: 168px;
  }
  .ip-thumb.static { cursor: default; }
  /* 铺满气泡的那一档：边框与底色都不要，圆角跟气泡（宿主的 border-radius）走 ——
     `inherit` 拿到的就是宿主那条（含左下/右下 4px 的那一角）。图片由 `overflow: hidden`
     裁成同一个圆角，所以气泡边缘不会露出自己的一圈白边。 */
  .ip-thumb.flush {
    border: 0;
    border-radius: inherit;
    background: none;
  }
  .ip-thumb img {
    display: block;
    width: auto;
    height: auto;
    max-width: 168px;
    max-height: 168px;
    object-fit: contain;
  }
</style>