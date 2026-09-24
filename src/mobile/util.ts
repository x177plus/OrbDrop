import type { IconName } from "./icons";

/**
 * 文件类型那一档：图标底色按它选（原型 `kindOf` 的同一套判据）。
 * 目录没有扩展名可看 —— 目录那一档由调用方自己给（`isDir ? 'folder' : kindOf(name)`），
 * 因为原型里目录的底色是一档、图标是另一套，两件事不共用一条判断。
 */
export type FileKind = "image" | "zip" | "doc";

const IMAGE = ["png", "jpg", "jpeg", "gif", "webp", "svg", "heic"];
const ZIP = ["zip", "rar", "7z", "tar", "gz"];

export function kindOf(name: string): FileKind {
  const ext = name.split(".").pop()?.toLowerCase() ?? "";
  if (IMAGE.includes(ext)) return "image";
  if (ZIP.includes(ext)) return "zip";
  return "doc";
}

/** 文件/目录各用哪颗图标：目录优先于扩展名（同一份判断在列表、气泡、选择框三处都要一致）。 */
export function fileIcon(name: string, isDir: boolean): IconName {
  if (isDir) return "folder";
  const k = kindOf(name);
  return k === "image" ? "image" : k === "zip" ? "zip" : "file";
}