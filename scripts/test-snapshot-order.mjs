// 会话快照的顺序回归：**旧的那份不能盖掉新的那份**。
//
// 背景（真机偶现）：发一条消息时后端落账那一刻记的是「发送中」，后台 HTTP 那一跳落定后才
// 推「已送达」。命令返回（IPC 响应）与 `chat://changed`（`eval` 进来的脚本）是两条独立通道，
// 谁先到没有保证 —— 旧返回晚到就会把「已送达」盖回「发送中」，消息一直停在那儿，直到关掉
// 会话再重新拉一次整份状态。修法是 `stores/chat.svelte.ts::replace` 按 `rev` 丢掉旧快照；
// 这个脚本用一个桩起来的 Tauri IPC 把那个先后顺序**确定性地**摆出来。
//
// 跑法：npm run test:snapshot
import { createServer } from 'vite';

const callbacks = new Map();
let nextId = 1;
// `chat_snapshot` 的返回由这个变量控制：测"晚到的命令返回"时，先把它设成旧快照。
let pendingSnapshot = { rev: 0, convos: [], batches: [], groups: [], pendingGroups: [], unread: 0 };

globalThis.window = globalThis;
globalThis.__TAURI_INTERNALS__ = {
  transformCallback(fn) {
    const id = nextId++;
    callbacks.set(id, fn);
    return id;
  },
  unregisterCallback(id) {
    callbacks.delete(id);
  },
  convertFileSrc: (p) => p,
  async invoke(cmd) {
    if (cmd === 'plugin:event|listen') return nextId++;
    if (cmd === 'plugin:event|unlisten') return undefined;
    if (cmd === 'chat_snapshot') return pendingSnapshot;
    if (cmd === 'list_devices') return [];
    if (cmd === 'list_device_remarks') return {};
    return null;
  },
};

const snapshot = (rev, delivery) => ({
  rev,
  convos: [
    {
      peerId: 'p',
      messages: [{ id: 'm1', fromMe: true, at: 1, text: 'hi', targets: ['p'], delivery }],
      files: [],
    },
  ],
  batches: [],
  groups: [],
  pendingGroups: [],
  unread: 0,
});

const server = await createServer({
  configFile: 'vite.config.js',
  server: { middlewareMode: true },
  appType: 'custom',
  logLevel: 'error',
});

let failures = 0;
const check = (name, ok, detail) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok ? '' : ` —— ${detail}`}`);
  if (!ok) failures += 1;
};

try {
  const chat = await server.ssrLoadModule('/src/lib/stores/chat.svelte.ts');
  // 模块加载时已经挂好 `chat://changed` 监听（`initListeners`）。
  await new Promise((r) => setTimeout(r, 50));
  const emit = (payload) => {
    for (const cb of callbacks.values()) cb({ event: 'chat://changed', id: 0, payload });
  };
  const labelOf = () => chat.deliveryText('p', chat.itemsOf('dev:p').find((i) => i.kind === 'text'));

  // 1) 新事件先到（已送达），旧返回后到（发送中）：旧的那份必须被丢掉。
  emit(snapshot(11, 'sent'));
  pendingSnapshot = snapshot(10, 'sending');
  await chat.refresh();
  check('旧命令返回不覆盖更新的已送达', labelOf() === '已送达', `实际是「${labelOf()}」`);

  // 2) 新返回后到（已送达）：正常路径照旧生效。
  pendingSnapshot = snapshot(12, 'sent');
  await chat.refresh();
  check('更新的快照照常生效', labelOf() === '已送达', `实际是「${labelOf()}」`);

  // 3) 已经到「已送达」之后，**再晚到的旧事件**同样不能把它推回「发送中」。
  emit(snapshot(9, 'sending'));
  check('旧事件不回退状态', labelOf() === '已送达', `实际是「${labelOf()}」`);
} finally {
  await server.close();
}

if (failures) {
  console.error(`\n${failures} 项失败`);
  process.exit(1);
}
console.log('\n全部通过');