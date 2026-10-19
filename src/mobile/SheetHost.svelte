<script lang="ts">
  import type { Sheet } from "./store.svelte";
  import PairSheet from "./sheets/PairSheet.svelte";
  import PairInviteSheet from "./sheets/PairInviteSheet.svelte";
  import LinkRequestSheet from "./sheets/LinkRequestSheet.svelte";
  import AddDeviceSheet from "./sheets/AddDeviceSheet.svelte";
  import DeviceActionsSheet from "./sheets/DeviceActionsSheet.svelte";
  import RemarkDeviceSheet from "./sheets/RemarkDeviceSheet.svelte";
  import FileActionsSheet from "./sheets/FileActionsSheet.svelte";
  import ChatActionsSheet from "./sheets/ChatActionsSheet.svelte";
  import AttachSheet from "./sheets/AttachSheet.svelte";
  import AppPickerSheet from "./sheets/AppPickerSheet.svelte";
  import ConvoSheet from "./sheets/ConvoSheet.svelte";
  import EditRecipientsSheet from "./sheets/EditRecipientsSheet.svelte";
  import KeepAliveSheet from "./sheets/KeepAliveSheet.svelte";
  import UpdateSheet from "./sheets/UpdateSheet.svelte";

  // 底板宿主：把 store 里那个槽位翻成具体的 sheet。
  // 这里是唯一"哪张 sheet 对应哪个组件"的地方 —— 各 tab 只 `openSheet({ kind: ... })`。
  let { sheet, onClose }: { sheet: Sheet; onClose: () => void } = $props();
</script>

{#if sheet.kind === "pair"}
  <PairSheet {onClose} />
{:else if sheet.kind === "pairInvite"}
  <PairInviteSheet {onClose} />
{:else if sheet.kind === "linkRequest"}
  <LinkRequestSheet {onClose} />
{:else if sheet.kind === "addDevice"}
  <AddDeviceSheet {onClose} />
{:else if sheet.kind === "deviceActions"}
  <DeviceActionsSheet peerId={sheet.peerId} {onClose} />
{:else if sheet.kind === "remarkDevice"}
  <RemarkDeviceSheet peerId={sheet.peerId} {onClose} />
{:else if sheet.kind === "fileActions"}
  <FileActionsSheet entry={sheet.entry} {onClose} />
{:else if sheet.kind === "chatActions"}
  <ChatActionsSheet convoKey={sheet.key} {onClose} />
{:else if sheet.kind === "attach"}
  <AttachSheet {onClose} />
{:else if sheet.kind === "appPicker"}
  <AppPickerSheet {onClose} />
{:else if sheet.kind === "convo"}
  <ConvoSheet {onClose} />
{:else if sheet.kind === "editRecipients"}
  <EditRecipientsSheet convoKey={sheet.key} {onClose} />
{:else if sheet.kind === "keepAlive"}
  <KeepAliveSheet {onClose} />
{:else if sheet.kind === "update"}
  <UpdateSheet {onClose} />
{/if}