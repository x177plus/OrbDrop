export interface ShareItem {
  id: string;
  name: string;
  path: string;
  createdAt: string;
  broadcastEnabled: boolean;
  /** 真元数据（后端 `ShareItemDto.isDir`），不再按路径猜。 */
  isDir: boolean;
  /** 文件大小；目录是 null。 */
  size: number | null;
  /** 目录的**直接**子项数；文件是 null。 */
  itemCount: number | null;
}

/** 对端设备标签里看到的共享项：只有展示所需的字段（没有对端本机绝对路径）。 */
export interface PeerShareItem {
  id: string;
  name: string;
  createdAt: string;
  /** 目录才能展开。 */
  isDir: boolean;
  /** 老版本对端不带这两个字段（那时是 `undefined`）。 */
  size?: number | null;
  itemCount?: number | null;
}

/** 没连着的时候"为什么"（后端 `LinkState`）。`connected` 为真时忽略它。 */
export type LinkState =
  | { kind: 'connected' }
  | { kind: 'retrying' }
  /** 对端此刻不在网上：等它出现（不再空发 TCP）。 */
  | { kind: 'waiting' }
  | { kind: 'stopped'; reason: 'peerDisconnected' | 'tokenRejected' | 'identityChanged' | 'localDisconnected' };

export interface PeerDevice {
  id: string;
  name: string;
  ip: string;
  port: number;
  online: boolean;
  paired: boolean;
  /** WS long connection is live (paired only) — this is what "长连接" means. */
  connected: boolean;
  /**
   * 此刻**看得见**这台设备吗：收到过它的 mDNS 记录、且记录还没被撤。
   *
   * 与 `online` 不是一回事（`online` = 探活成功 = 真连得上）。这一位**只干两件事**：
   * ① 离线且未绑定的那一行照旧列出来（否则"刚才还看到它、现在没了"）；② 发起连接时
   * 说不说那句「直连不通」的 tip（`unicastBlocked` / `unicastBlockedTip`）。
   * **它不再是一个状态档**：那一行的胶囊就是「未绑定」/「离线」，点照旧按 `offline` 画。
   */
  seen: boolean;
  /**
   * 非空 = 此刻**经这台设备中转**（值是中转方的名字，后端 `relay_via`）：
   * 行上加一句小字「经 <名字> 中转」，用户才知道这条路不是直连。
   */
  relayVia: string | null;
  /**
   * F-107：**这一段设备的中转记忆**（有的话）——`viaCidr` = 中转网段，`poolSize` = 池里有几台。
   *
   * 与本行的 `relayVia`（此刻经谁中转）是两件事：这一位是"这段装不通时该走哪一段"的
   * 持久策略（`config.toml` 的 `relayRoutes`），所以它在这一行**即使当前直连通着**也会在。
   * 行上只说一句小字（`transitHint`），别把它做成状态档。
   */
  transit?: { viaCidr: string; poolSize: number } | null;
  /** 自动重连的现状；缺省 = 没有拨号意图（已配对但没人点过「连接」）。 */
  linkState?: LinkState | null;
  /**
   * 本机给这台设备起的备注（F-101）：`null`/缺省 = 没备注。
   *
   * **只存在本机**（`config.toml` 的 `deviceRemarks`，键是设备 id）：不随设备名广播出去，
   * 对端看到的始终是它自己的名字。界面上设备名一律显示成「备注(设备名)」——
   * 拼装只有一处（`stores/devices` 的 `deviceDisplayName`），别在组件里各拼一套。
   */
  remark?: string | null;
}

export interface AppSettings {
  port: number;
  /** 默认的落盘目录：手动下载（受 downloadMode 影响）与推送接收都默认写这里；
   *  「每次询问」时接收未配对设备的推送会先挑一个目录，那次就用挑的那个作根。 */
  downloadDir: string;
  /** `AskEachTime` = 下载前弹保存框；接收未配对设备的推送前也弹一次目录框（取消即拒绝）。
   *  已配对设备的推送免同意自动收，与它无关。 */
  downloadMode: 'Direct' | 'AskEachTime';
  deviceName: string;
  autoStart: boolean;
  /** BeyondCompare 可执行文件路径；'' = 自动探测。 */
  bcExePath: string;
  /**
   * 本机「在线 / 离线」（面板右上角状态框）。`false` = 隐身：不广播 mDNS、断开全部
   * 长连接、拒收一切入站请求。落盘、跨重启有效。
   */
  online: boolean;
  /**
   * 折叠态悬浮形态：`circle` = 62px 圆球（`Orb.svelte`）；`rect` = 圆角矩形悬浮框
   * （`RectOrb.svelte`，`design/orbdrop-rect-orb.html`）。落盘、跨重启有效。
   */
  orbShape: OrbShape;
  /**
   * 新添加的目录/文件是否**默认开播**（设置里的「添加后自动共享」，默认 `false`）。
   *
   * 只作用于之后新登记的共享项：后端 `add_share`（`add_share_impl`）建条目时把这个值抄进
   * `broadcastEnabled` 的初值，已经在列表里的开关一个都不动。
   */
  autoBroadcastNew: boolean;
  /**
   * 本机是否替群聊**中转**消息（设置里的「允许为群聊中转消息」，默认 `false`）。
   *
   * 群里两个成员互相够不着时（跨网段 / 只有本机跟它们都连得上），本机把消息替它们转一份，
   * 最多 3 跳（后端 `hops <= 3` + `reached` 抑制重复）。**默认关**：代价落在本机 ——
   * 每转一份就占一份上行带宽，而"转不转"是用户自己的取舍，不该由程序替他决定。
   */
  groupRelay: boolean;
  /**
   * 本机是否为其它设备**盲转字节**（设置里的「允许为其它设备中转」，默认 `false`）。
   *
   * 某些 AP 把客户端之间的流量隔开，两台设备单播不通但都连得上本机 —— 本机替它们搬字节
   * （两端 TLS 端到端不变，本机看不到内容）；请求方也只在直连不通时才回退走中转。
   * **默认关**：代价落在本机 —— 每转一份就占一份上行带宽，该不该出由用户自己定。
   */
  allowRelay: boolean;
  /**
   * 本机是否**记住并自动复用跨网段中转**（设置里的「记住并自动复用跨网段中转」，默认 `true`，F-107/S-10）。
   *
   * 开着：直连探不通的设备一旦经中转真的到达，本机就把「这一段设备经哪一段中转」记进地址簿
   * （设备级链路 + 网段级策略 + 中转池），同段的其他设备直接复用；C 掉线自动换池内的另一台；
   * 并把这条路由推给被中转的那一侧。
   *
   * 关着 = **完全退回 F-106**：端点每次现开、用完即收，不落盘、不泛化、不传播。
   *
   * **默认开**（与上面几条"默认关"不同）：这一条不替别人做事、也不向对端暴露本机的新能力
   * （花带宽、暴露能力的那一侧由中转方的 `allowRelay` 把关），只影响本机自己的选路效率。
   */
  relayMemory: boolean;
  /**
   * 接收推送时的**传输并发数**（设置里的「传输并发数」，默认 4）：一次接收同时跑几路传输。
   * 后端 `config::clamp_transfer_workers` 把取值域夹在 1..=8 —— 再高只会把连接、句柄与磁盘队列都堆起来。
   */
  transferWorkers: number;
  /**
   * 大文件的**分块大小**（MB，设置里的「分块大小（MB）」，默认 4）：大文件按它切片、片与片之间并行。
   * 后端 `config::clamp_transfer_block_mb` 把取值域夹在 1..=32。
   */
  transferBlockMb: number;
}

/** 折叠态悬浮形态：圆球 / 圆角矩形悬浮框（后端 `config::OrbShape` 的同一套小写字面量）。 */
export type OrbShape = 'circle' | 'rect';

/**
 * F-106 / S-09：本机替别人中转的**累计流量记录**（`relay_traffic` 命令；后端
 * `AppState::relay_traffic_json`）。
 *
 * 中转方的唯一代价就是上行带宽，设置里就显示这两个数（"替别人搬了多少"）。字节数两处合流：
 * 盲转的连接字节 + 替群聊转发出去的消息字节。它是落了盘的记录（`config.toml` 的
 * `relayBytesTotal` / `relayConnsTotal`），重启接着算。
 */
export interface RelayTraffic {
  /** 收发两个方向之和的字节数（盲转连接 + 替别人转发的群消息）。 */
  bytes: number;
  /** 累计中转连接数（只数盲转的连接，不含消息中转）。 */
  connections: number;
}

/**
 * 会话模式（原型的三张身份）：一对一 / 单向群发 / 共享群聊。
 *
 * - `direct`：与一台设备的那条（后端 `ChatPeerConvo`）；
 * - `broadcast`：一次发给几台，各台单独收到 —— 后端由「一批收件人」的名单 + 副本上的
 *   `batchId` 支撑（`ChatBatch`）；
 * - `group`：共享群聊 —— 一条**所有人的同一个**消息流（`ChatGroup`）；收到邀请还没点「加入」
 *   的群不算群，只在 `ChatSnapshot.pendingGroups` 里。
 */
export type ConvoMode = 'direct' | 'broadcast' | 'group';

/**
 * BeyondCompare 比较的一端：本机共享项 / 长连接设备的共享项。
 * `path` 是共享项内相对路径（'' = 共享项根）。
 * `label` 是给人看的名字，由前端填（对端共享项的名字只有前端知道）；后端优先采用它。
 */
export type BcTarget =
  | { kind: 'share'; shareId: string; path: string; label?: string }
  | { kind: 'peer'; peerId: string; shareId: string; path: string; label?: string };

/** 比较位上的项。 */
export interface BcSide {
  target: BcTarget;
  label: string;
}

/** 后端返回的 BeyondCompare 状态：左/右比较位为空时对应字段为 null。 */
export interface BcStatus {
  exePath: string | null;
  left: BcSide | null;
  right: BcSide | null;
}

/**
 * 收件目录里的一项（后端 `mobile::InboxEntryDto`，只在移动端构建里注册）。
 *
 * `from` 是落盘时那一层发送方目录名；本机自己下载下来的为空串（它们直接落在下载目录下）。
 */
export interface InboxEntry {
  name: string;
  path: string;
  isDir: boolean;
  /** 目录恒为 0：不为它递归求和，界面按 `isDir` 显示「文件夹」。 */
  size: number;
  /** epoch 毫秒。 */
  modified: number;
  from: string;
}

export interface TreeEntry {
  name: string;
  isDir: boolean;
  size: number;
  modified: string;
}

export interface TreeResponse {
  entries: TreeEntry[];
}

/** 选择框顶部「常用」的一条（配置里的 `pickerFavorites`）：用户右键目录攒的快捷入口。 */
export interface PickerFavorite {
  name: string;
  path: string;
}

/** 「设备与驱动器」里的一项（后端 `FsDrive`）。 */
export interface FsDrive {
  name: string;
  path: string;
  /** 可移动磁盘（U 盘、移动硬盘）。 */
  removable: boolean;
  /** 总容量 / 可用空间（字节）。取不到时为 0，界面据此不画占用条。 */
  total: number;
  free: number;
}

export interface FsEntry {
  name: string;
  path: string;
  isDir: boolean;
  /** 文件夹固定 0：列一层不读它的子项。 */
  size: number;
}

/** 面包屑的一段：`path` 是点它要跳到的绝对路径。 */
export interface FsCrumb {
  name: string;
  path: string;
}

/** 一层目录的内容（后端 `fs_list`）。遍历一律走 `crumbs` + `entries` 的 `path`，前端不拼路径。 */
export interface FsListing {
  path: string;
  crumbs: FsCrumb[];
  entries: FsEntry[];
}

/** 列举失败（后端 `FsError`）：`kind` 选状态页，`crumbs` 保证失败时面包屑也是对的。 */
export interface FsError {
  kind: 'denied' | 'missing' | 'notDir' | 'other';
  message: string;
  crumbs: FsCrumb[];
}

/** 选择框里选中并交给调用方的一项（推送直接拿 `path` 拼一批路径）。 */
export interface LocalPick {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
}

/**
 * 拖拽下载的源（F-46）：本机共享项（省略 `peerId`）或对端共享项。
 * `path` 是共享项内相对路径（'' = 共享项根），`name` 是拖出去那一项的名字 ——
 * 对端共享项根的名字本机不知道（只有 id），只能由界面带下来。
 */
export interface DragOutSource {
  peerId?: string;
  shareId: string;
  path: string;
  name: string;
  isDir: boolean;
}

/** 拖拽票据：`ready` = false 表示后端还在准备对端项（走目录，必要时先下到暂存目录）。 */
export interface DragTicket {
  ticket: string;
  name: string;
  ready: boolean;
}

export interface PushInvite {
  inviteId: string;
  sender: { id: string; name: string };
  name: string;
  size: number;
  isDir: boolean;
  /** 发送方这次选了几项（>1 = 多选合并成的一条推送）。 */
  count: number;
  /**
   * 后端已经**自动收下**了这条推送（发送方是本机已绑定设备：token 就是那份信任，
   * 不再问用户一次）。界面据此直接显示"正在接收"，不放「接收/拒绝」。
   */
  auto?: boolean;
}

/** 一台目标设备在一批推送里的状态（后端 `TargetState`）。 */
export type PushTargetState = 'invited' | 'sending' | 'done' | 'cancelled' | 'rejected' | 'failed';

/** 一批推送里的一台目标设备（卡片上的一行，后端 `BatchTargetView`）。 */
export interface PushTarget {
  peerId: string;
  /** 设备名。 */
  target: string;
  inviteId: string;
  /** 已配对 = 对方不问同意直接收：行文案别说「等待对方同意」。 */
  paired: boolean;
  state: PushTargetState;
  received: number;
  error: string | null;
}

/**
 * 一次推送操作（= 界面上一张卡片，后端 `BatchView`）。单台推送也是一批（只有一条目标），
 * 所以"等待对方同意 → 传输中 → 已发送"和"3 台里成了几台"是同一条数据。
 */
export interface PushBatch {
  batchId: string;
  /** 清单的显示名（"推的是什么"），不参与落盘路径。 */
  name: string;
  /** 这次选了几项（>1 = 多选合并成的一条推送）。 */
  count: number;
  /** 清单里被跳过的项数（链接 / 读不到的项）。>0 时卡片要说出来，别再让用户以为"就这些"。 */
  skipped: number;
  /** 成功 / 等待（含传输中）/ 失败（含被拒、被取消）—— 卡片上「成功 x · 等待 y」那行就是它。 */
  done: number;
  waiting: number;
  failed: number;
  /** 聚合进度：分母只算还活着的台（失败/取消的整条不计），所以有台失败也能走到 100%。 */
  received: number;
  total: number;
  targets: PushTarget[];
}

/**
 * 会话（聊天）里一条消息在一台设备上的状态（后端 `chat::Delivery`）。
 *
 * `sending` = 在途（没有长连接，正走 HTTP 兜底那一跳）；`sent` = 已交给对端（写进长连接，
 * 或 HTTP 拿到了 200）；`read` = 对端回执说它读过了；`queued` = 对端这会儿不在网上，
 * **排队等它上线**（它一上线就按原顺序自动发出，消息没丢）；`failed` = 对端明确拒绝
 * （4xx：认不出凭据或请求体 —— 重试不会变好，不发第二次）；`received` = 收到的消息 ——
 * 本机视角没有"送达"可言。
 *
 * `synced` = **只有群消息有**：这一份不是本机直接投到对方的，是对方后来从**别的成员**那里
 * 补到的（补拉 / 跨段转发；后端 `Delivery::Synced`，由 `GroupAck` 回过头告诉原发送方）。
 * 内容与直投的那份是同一份，只是路径不同 —— 所以它算"到位"，不算没送到。
 */
export type ChatDelivery = 'sending' | 'sent' | 'read' | 'synced' | 'queued' | 'failed' | 'received';

/**
 * 会话里的一条文本消息。
 *
 * 群发时**每台设备各存一份副本、共用同一个 `id`**（各自落在与该设备的 1:1 会话里）：
 * "送到了几台"就是对着 `targets` 逐台看副本的 `delivery`。
 */
export interface ChatMessage {
  id: string;
  /** 本机发出的 = true（气泡靠右）；对端发来的 = false（靠左，`fromName` 说明是谁说的）。 */
  fromMe: boolean;
  /** 对端消息的发送方名字；本机发出的为空串。 */
  fromName: string;
  text: string;
  /** 发送方的墙上时间（epoch 毫秒）：排序与已读水位都按它。 */
  at: number;
  delivery: ChatDelivery;
  targets: string[];
  /** 这条是走哪个群发发出去的（`null`/缺省 = 一对一发的）。 */
  batchId?: string | null;
}

/**
 * 一批收件人（会话列表里的「群发 · 名字」）。
 *
 * 它只是一份**名单**，不拥有消息：群发的记录 = 副本上带着这个 `id` 的那些消息
 * （`ChatMessage.batchId`）。所以删掉名单不会丢消息，成员变了也不动历史。
 */
export interface ChatBatch {
  id: string;
  /** 显示名（`群发 · <名字>`）。空 = 界面退回「群发 · N 台」。 */
  name: string;
  /** 成员设备 id（用户选择的顺序，已去重）。 */
  members: string[];
}

/** 一台设备的会话：会话就是**与这台设备**的那一条（没有群实体，群发只是每台各存一份）。 */
export interface ChatPeerConvo {
  peerId: string;
  messages: ChatMessage[];
  /** 本机已读水位（epoch 毫秒）：`at <= readAt` 的对端消息算读过。 */
  readAt: number;
  unread: number;
}

/** 群成员（名册里的一行）。 */
export interface ChatGroupMember {
  id: string;
  name: string;
}

/**
 * 一个共享群聊。
 *
 * 与群发**相反**：群聊只有**一条**消息流（`messages` 就是它），群消息**不镜像进各台的
 * 1:1 会话里** —— 界面上只出现在 `grp:<群 id>` 那一条视图里。名册的真相在后端（含本机），
 * 这里这份 `members` 是**视图**：后端已经把自己滤掉了，前端不需要知道本机设备 id。
 */
export interface ChatGroup {
  id: string;
  /** 群名；空 = 标题回落到成员名（与 `titleOf('group', …)` 一致）。 */
  name: string;
  /** 名册版本（LWW 用，前端只读）。 */
  rev: number;
  /** 创建者设备 id。 */
  createdBy: string;
  /** 名册（**不含本机**，后端已滤掉；顺序按名册）。 */
  members: ChatGroupMember[];
  /** 群消息，落盘顺序（追加序）。 */
  messages: ChatGroupMsg[];
  /** 本机读到哪条了（那条消息 id；`null` = 一条都没读过）。 */
  readPos: string | null;
  /** 本群的未读：本机之外的成员发的、排在 `readPos` 之后的条数（后端算好，前端别重算）。 */
  unread: number;
  /**
   * 本机**已知缺**在群里的条数（后端算好，前端别重算）：掉线期间别人说的话，补拉又够不着的
   * 那一部分（`seq` 的洞 + `since_at` 之后的缺口，见 `GroupSyncAsk`）。
   *
   * `> 0` 时消息流头顶如实说一句「有 N 条消息未能同步」—— 缺口是有的就说，不假装完整，
   * 也不为它摆一个"正在同步"的假状态（补拉是尽力而为，一次没拉到不代表下次拉不到）。
   */
  missing: number;
}

/** 群里的一条消息。 */
export interface ChatGroupMsg {
  id: string;
  /** 发送方设备 id（本机发的 = 本机自己的 id）。 */
  from: string;
  /** 发送方显示名（本机发出的为空串）—— 颜色**不要**跟着它走，改一次名就会跳。 */
  fromName: string;
  text: string;
  at: number;
  /** 该发送方在这个群里的第几条（1 起）：只用于同一个人说话的先后。 */
  seq: number;
  fromMe: boolean;
  /**
   * **仅 `fromMe` 有意义**：成员 id → 状态（`queued`/`sending`/`sent`/`read`/`synced`/`failed`）；
   * 收来的那条是空对象（别人发到没到不是本机的事）。
   */
  delivery: Record<string, ChatDelivery>;
  /** **仅 `fromMe` 有意义**：读过这条的成员数（配 `delivery` 的台数就是「已读 n/m」）。 */
  readCount: number;
}

/** 收到但还没确认的群邀请：用户没点「加入」之前它都不算群（只进会话列表那张邀请卡）。 */
export interface PendingGroup {
  /** = 群 id。 */
  id: string;
  name: string;
  /** 邀请人设备 id。 */
  from: string;
  fromName: string;
  /** 名册（不含本机）。 */
  members: ChatGroupMember[];
  /** 邀请到达时刻。 */
  at: number;
  /**
   * 邀请挂着期间已经到门口的消息条数（后端 `orphans` 里现数的，后端算、界面不算）。
   * 点「加入」那一刻它们并进会话 —— 卡上那句「已有 N 条消息在等你」就是它。
   */
  buffered: number;
}

/** 整份会话状态：`chat_snapshot` 命令的返回，也是 `chat://changed` 事件的载荷。 */
export interface ChatSnapshot {
  convos: ChatPeerConvo[];
  /** 群发名单（会话列表里「群发 · 名字」那些行就是它）。 */
  batches: ChatBatch[];
  /** 本地已知的群（已加入的那些；含本地拒绝邀请之后的状态）。 */
  groups: ChatGroup[];
  /** 收到但还没确认的群邀请（点了「加入」才进 `groups`）。 */
  pendingGroups: PendingGroup[];
  /** 全部未读（底栏那颗角标 / 悬浮球左上角那颗）：含群聊未读。 */
  unread: number;
}

/**
 * 悬浮球起拖那一刻的按下点（CSS px，窗口坐标 = 折叠态窗口的视口坐标）。
 *
 * `App.beginOrbDrag` 把它交给后端：后端按它把窗口的抓取锚点校正回按下点，再当场起拖。
 */
export interface OrbDragInfo {
  /** 按下时的指针位置 */
  pressX: number;
  pressY: number;
}

/** 圆角矩形悬浮框里的一格：一台设备的共享项（或钻入子目录后的一个子项）在卡上的样子。 */
export interface RectSlot {
  /** 共享项 id：拖出（`DragOutSource.shareId`）与打开（`openPath` / `download_from_peer`）都用它定位。 */
  id: string;
  /** 共享项内的相对路径（`''` = 共享项根）：拖出与打开都按「共享项 + 相对路径」定位。 */
  path: string;
  name: string;
  isDir: boolean;
  /** 第二行的等宽小字（卡上只放得下很短的一段）。 */
  meta: string;
}

/**
 * 列表头上那枚「↰ 上一级」（只有钻进了子目录才在）：双击 = 回上一层，名字是**父目录**
 * 的名字（回根那一层时是共享项自己的名字）。
 */
export interface RectUp {
  name: string;
  /** 第二行小字，固定说清这一格是干什么的。 */
  meta: string;
}

/**
 * 圆角矩形悬浮框的一页：**第一页永远是本机**，其后每台长连接设备一页。
 *
 * 设备页只有长连接设备才有：对端的共享项**只有在本机与它保持长连接时才对本机可见**
 * （F-25 的广播范围），离线/未配对的设备根本没有可列的东西。本机页则一直在，
 * 内容就是「本机」标签页那份共享项清单。
 */
export interface RectPage {
  /** 稳定 key：本机页 = `'local'`，设备页 = peerId。 */
  id: string;
  /** 这一页是谁：本机的共享项 / 一台设备对本机可见的共享项。 */
  kind: 'local' | 'peer';
  /** 头部第一行。本机页固定是「本机」（不写设置里那个设备名，见 `App.rectPages` 的说明）。 */
  name: string;
  /** `ip:port`：卡片头部第二行的左半（本机页是自己那个地址；取不到时为空）。 */
  ip: string;
  /** 这一页还在拉取中（设备页首屏那一瞬）。 */
  loading: boolean;
  /** 拉取失败的原因（未配对 / 对端不可达）：卡片里说清楚，而不是显示成一个空列表。 */
  error: string | null;
  /**
   * 钻入子目录后**现在在哪**：`label` 是给 176px 宽的卡片量过的显示串（长了从头上砍，
   * 尾巴才是"我在哪"），`full` 给悬停 `title`。`null` = 这一页在看共享项清单，
   * 头部第二行照旧说 `ip:port`。
   */
  dir: { label: string; full: string } | null;
  /** 「↰ 上一级」那一格（`null` = 没钻入）。**不算进 `items`**：项数说的是"这里有几项"。 */
  up: RectUp | null;
  items: RectSlot[];
}

/** 一级标签栏的标签：本机 / 设备（关系管理） / 每台长连接设备一个浏览标签（F-24、F-30）。 */
export type TabKey =
  | { kind: 'local' }
  | { kind: 'devices' }
  | { kind: 'peer'; peerId: string };

export function tabKeyEquals(a: TabKey, b: TabKey): boolean {
  if (a.kind !== b.kind) return false;
  return a.kind === 'peer' && b.kind === 'peer' ? a.peerId === b.peerId : true;
}

export function tabKeyId(tab: TabKey): string {
  return tab.kind === 'peer' ? `peer:${tab.peerId}` : tab.kind;
}

/**
 * 共享项第二行（卡片格、「本机」/设备标签页）那一段小字：目录给「N 项」，文件给大小。
 *
 * 两个数都由后端问一次文件系统得来（本机 `ShareItemDto`、对端 `/shares`），拿不到时是
 * `null` —— 那时只说「文件夹 / 文件」：**宁可不说，也不报一个没根据的数字**（上一版这里显示
 * 的是共享项的登记日期，连对端看到的都是"别人哪天加进来的"）。
 *
 * 本机共享项与对端共享项共用这一份：同一批共享项不该在两处长得不一样。
 */
export function shareMeta(
  isDir: boolean,
  itemCount: number | null | undefined,
  size: number | null | undefined,
): string {
  if (isDir) return typeof itemCount === 'number' ? `${itemCount} 项` : '文件夹';
  return typeof size === 'number' ? formatFileSize(size) : '文件';
}

export function formatFileSize(bytes: number): string {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

export function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch { return iso; }
}

/**
 * `chat://undeliverable` 事件的载荷（后端 `relay::notify_undeliverable`，F-106）：
 * 一条消息**直连不通、也没有可用的中转**时发出来（消息已按既有语义排队，不丢）。
 */
export interface Undeliverable {
  peerId: string;
  /** 目标设备的显示名（后端给的是它自报的名字，界面上要「」框住）。 */
  name: string;
  /** `no_relay` = 找不到愿意中转的设备；`relay_failed` = 有候选但这次没转成。 */
  reason: 'no_relay' | 'relay_failed';
  /** 目标此刻"看得见"吗（mDNS 记录还在）——据此补一句"可能这张 Wi-Fi 隔离了客户端"。 */
  seen: boolean;
}

/**
 * 一条消息暂时送不出去时给用户的那句话（**桌面与移动共用同一份文案**）。
 *
 * 以前是静默的：消息照排队，界面上却什么都没说，用户只看到"发了没反应"。这里只补解释 ——
 * 排队本身是既有语义（等它上线再发，不丢）。
 */
export function undeliverableText(u: Undeliverable): string {
  const why = u.reason === 'no_relay' ? '没有可用的中转方' : '中转方这次没转成';
  let text = `发给「${u.name}」的消息暂时送不出去：${why}。已排队，等它上线会自动发出。`;
  if (u.seen) {
    text += '它在同一网段，可能这张 Wi-Fi 隔离了客户端 —— 让一台两端都够得着的设备打开设置里的「允许为其它设备中转」就能自动接管。';
  }
  return text;
}