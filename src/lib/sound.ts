/**
 * 新消息提示音。
 *
 * 用 Web Audio 现场合成一小段「叮—咚」，不带音频资源：两个正弦音 + 指数衰减就够，
 * 体积为零，桌面端与安卓端共用同一份（打包路径、循环播放都不必操心）。
 *
 * 浏览器（含 WebView2）默认在用户第一次交互前挂起 `AudioContext`。模块加载时挂一对
 * 一次性监听，第一次点击 / 按键就把上下文解锁；桌面端另在 `tauri.conf.json` 里关掉了
 * WebView2 的自动播放手势限制，所以托盘里没点过窗口也能响。拿不到上下文或它仍挂起时
 * 静默跳过 —— 提示音不该成为 App 的错误源。
 */

/** 两条消息挨得太近时只响一声。 */
const RATE_LIMIT_MS = 180;

let ctx: AudioContext | null = null;
let lastPlayed = 0;

function audioContext(): AudioContext | null {
  if (ctx) return ctx;
  if (typeof window === 'undefined') return null;
  const Ctor =
    window.AudioContext ??
    (window as Window & { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
  if (!Ctor) return null;
  try {
    ctx = new Ctor();
  } catch {
    return null;
  }
  return ctx;
}

/** 在用户手势里调一次即可解锁音频（自动播放策略）；重复调用无副作用。 */
export function unlockSound(): void {
  const c = audioContext();
  if (c && c.state === 'suspended') void c.resume().catch(() => {});
}

// `typeof window !== 'undefined'` 不够：测试/SSR 会把 `window` 桩成 globalThis（没有
// `addEventListener`）。模块顶层挂监听必须再确认这两个方法真的存在，否则整个 chat store
// 在加载时就会抛错（`npm run test:snapshot` 就是在这一步炸的）。
if (
  typeof window !== 'undefined' &&
  typeof window.addEventListener === 'function' &&
  typeof window.removeEventListener === 'function'
) {
  const unlock = () => {
    unlockSound();
    window.removeEventListener('pointerdown', unlock);
    window.removeEventListener('keydown', unlock);
  };
  window.addEventListener('pointerdown', unlock, { once: true, passive: true });
  window.addEventListener('keydown', unlock, { once: true });
}

/** 收到新消息时响一声；上下文没解锁 / 播放被限流时什么都不做。 */
export function playMessageSound(): void {
  const c = audioContext();
  if (!c || c.state !== 'running') {
    if (c) void c.resume().catch(() => {});
    return;
  }
  const now = performance.now();
  // 限流：几毫秒内连着来几条（连续快照）不叠成一串机关枪。
  if (now - lastPlayed < RATE_LIMIT_MS) return;
  lastPlayed = now;
  const at = c.currentTime + 0.01;
  chime(c, 880, at, 0.16, 0.12);
  chime(c, 1174.66, at + 0.085, 0.28, 0.1);
}

/** 一个带起音 / 衰减包络的正弦音。包络从极小值起落，避免爆音。 */
function chime(c: AudioContext, freq: number, at: number, dur: number, peak: number): void {
  const osc = c.createOscillator();
  const gain = c.createGain();
  osc.type = 'sine';
  osc.frequency.setValueAtTime(freq, at);
  gain.gain.setValueAtTime(0.0001, at);
  gain.gain.exponentialRampToValueAtTime(peak, at + 0.012);
  gain.gain.exponentialRampToValueAtTime(0.0001, at + dur);
  osc.connect(gain);
  gain.connect(c.destination);
  osc.start(at);
  osc.stop(at + dur + 0.03);
}