import * as api from '$lib/api';

/**
 * 运行平台。**只用于换文案与快捷键写法**，不参与任何功能分支 —— 后端已经按平台把能力
 * 做齐了，前端不需要知道「拖出是 OLE 还是 NSDraggingSession」。
 *
 * 初值是 `unknown`：设置面板/会话可能在 `runtimeInfo()` 回来之前就渲染了一帧。那几处文案
 * 会在这份状态更新后自动重算；功能不受影响。
 */
type RuntimeInfo = {
  os: 'windows' | 'macos' | 'linux' | 'android' | 'ios' | 'unknown';
  arch: string;
  desktop: boolean;
};

let info = $state<RuntimeInfo>({ os: 'unknown', arch: '', desktop: true });

export function platform() {
  return info;
}

/** 平台探测只成功一次；失败保持 `unknown`（退化成中性文案，不阻塞启动）。 */
export async function loadPlatform() {
  try {
    const got = await api.runtimeInfo();
    info = { os: got.os as RuntimeInfo['os'], arch: got.arch, desktop: got.desktop };
  } catch (e) {
    console.error('读取运行平台失败:', e);
  }
}

export function isMac() {
  return info.os === 'macos';
}

/** 文件管理器在文案里的名字。 */
export function fileManagerLabel() {
  return isMac() ? '访达' : info.os === 'windows' ? '资源管理器' : '文件管理器';
}

/** 文件选择器最外层那一层的名字（Windows 习惯叫「此电脑」）。 */
export function computerLabel() {
  return info.os === 'windows' ? '此电脑' : '本机';
}

/** 粘贴的快捷键写法。 */
export function pasteShortcut() {
  return isMac() ? '⌘V' : 'Ctrl+V';
}