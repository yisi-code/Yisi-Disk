import { useI18n } from 'vue-i18n'
import type { JobTarget } from '../services/host'

/**
 * 分区与来源的显示文字：文件系统名、分区用途、读分区表时的说明、来源，以及不能浏览的原因。
 * 找回删除的文件、找回丢失的分区、修复打不开的分区、检查文件系统共用，规则只有一份
 */
export function usePartitionText() {
  const { t, te } = useI18n()

  /** 文件系统的显示名；没有文案的原样显示 */
  function filesystemName(filesystem: string): string {
    const key = `partition.fs.${filesystem}`
    return te(key) ? t(key) : filesystem
  }

  /** 分区的用途：常见的给本地化名称，否则显示类型（MBR 类型字节或 GPT 类型 GUID），都没有时为 - */
  function roleName(partition: { role: string | null; typeId: string }): string {
    const key = `partition.role.${partition.role ?? ''}`
    if (partition.role !== null && te(key)) {
      return t(key)
    }
    return partition.typeId === '' ? '-' : partition.typeId
  }

  /** 读分区表时发现的问题 */
  function tableNote(note: string): string {
    const key = `undelete.notes.${note}`
    return te(key) ? t(key) : note
  }

  /** 来源：镜像文件的路径或磁盘编号 */
  function sourceName(source: JobTarget | null): string {
    if (source === null) {
      return ''
    }
    return source.kind === 'image'
      ? t('undelete.sourceImage', { path: source.path })
      : t('undelete.sourceDisk', { number: source.diskNumber })
  }

  /** 不能浏览的原因；文件系统认得出、却打不开时按「参数不对」说 */
  function unbrowsableReason(filesystem: string): string {
    const key = `undelete.partition.reason.${filesystem}`
    return te(key) ? t(key) : t('undelete.partition.reason.invalid')
  }

  return { filesystemName, roleName, tableNote, sourceName, unbrowsableReason }
}
