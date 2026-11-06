import type { FsEntry } from '../services/host'

/** 分区里的文件列出哪些项：未删除的、已删除的、带隐藏或系统属性的（三项各自勾选） */
export interface FileFilter {
  live: boolean
  deleted: boolean
  hidden: boolean
}

/**
 * 按 `filter` 这一项列不列：隐藏与系统文件没勾选时都不列（同资源管理器的默认）；只勾已删除时，
 * 未删除的文件夹照样列出（删除的文件可能在里面）。目录树与勾选项共用这一条规则
 */
export function isShownBy(filter: FileFilter, entry: FsEntry): boolean {
  if ((entry.hidden || entry.system) && !filter.hidden) {
    return false
  }
  if (entry.deleted) {
    return filter.deleted
  }
  return filter.live || (filter.deleted && entry.isDir)
}
