<script lang="ts">
  import Icon from "../Icon.svelte";
  import { fileIcon, kindOf } from "../util";

  let { name, isDir = false }: { name: string; isDir?: boolean } = $props();
</script>

<span class="ftile {isDir ? "folder" : kindOf(name)}"><Icon name={fileIcon(name, isDir)} /></span>