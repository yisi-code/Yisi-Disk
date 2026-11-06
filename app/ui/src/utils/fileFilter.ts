import type { FsEntry } from '../services/host'

/** 分区里的文件列出哪些项：未删除的、已删除的、带隐藏或系统属性的（三项各自勾选） */
export interface FileFilter {
  live: boolean
  deleted: boolean
  hidden: boolean
}

/** 这种勾选的编号（未删除 1、已删除 2、隐藏 4 相加），与引擎 `ShowFilter::index` 一致 */
function filterIndex(filter: FileFilter): number {
  return (filter.live ? 1 : 0) | (filter.deleted ? 2 : 0) | (filter.hidden ? 4 : 0)
}

/**
 * 按 `filter` 这一项列不列。规则只在引擎里（文件看自身；文件夹看里面有没有符合的项，本来就空的按自身），
 * 引擎为每一项算好各种勾选下的结果（`shownUnder`），这里只取当前勾选那一位。目录树与勾选项共用
 */
export function isShownBy(filter: FileFilter, entry: FsEntry): boolean {
  return (entry.shownUnder & (1 << filterIndex(filter))) !== 0
}
