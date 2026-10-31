<script lang="ts">
  import type { DeviceState } from "$lib/stores/devices.svelte";

  // 设备状态胶囊：档位与文案全部来自 `stores/devices` 的 `deviceState()`，移动端不自己算一套
  // （桌面端三处各算一套的教训）。
  //
  // 胶囊与状态点**分开取类**：胶囊说"这台现在是什么关系"（未绑定 / 已配对 / 离线…），
  // 点说"现在送得到吗、有没有实时链路"（实心 = 长连接，空心环 = 只是在线）。原型只有
  // mock 的三档，真数据是五档 —— 五档的配色与形状在 `mobile.css` 末尾按桌面端同一套语义给出。
  let { state }: { state: DeviceState } = $props();
</script>

<span class="state-chip {state.chip}" title={state.chip === "localsend" ? "LocalSend" : undefined}><i class="dot {state.dot} {state.trusted ? 'solid' : 'hollow'}"></i>{state.label}</span>