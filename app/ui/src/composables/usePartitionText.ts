import { useI18n } from 'vue-i18n'
import type { JobTarget } from '../services/host'
import { formatBytes, formatHex } from '../utils/format'

/** 引导扇区里按十六进制显示的字段 */
const HEX_BOOT_FIELDS = ['volumeSerial', 'volumeSerialHigh', 'mediaDescriptor', 'extFlags']

/**
 * 分区与来源的显示文字：文件系统名、分区用途、读分区表时的说明、来源，以及不能浏览的原因。
 * 找回删除的文件、找回丢失的分区、修复打不开的分区、检查文件系统共用，规则只有一份
 */
export function usePartitionText() {
  const { t, te, locale } = useI18n()

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
    const key = `partition.notes.${note}`
    return te(key) ? t(key) : note
  }

  /** 来源：镜像文件的路径或磁盘编号 */
  function sourceName(source: JobTarget | null): string {
    if (source === null) {
      return ''
    }
    return source.kind === 'image'
      ? t('partition.sourceImage', { path: source.path })
      : t('partition.sourceDisk', { number: source.diskNumber })
  }

  /** 磁盘的名字：厂商加型号，都没有时是「未命名的磁盘」 */
  function diskName(disk: { vendor: string; model: string }): string {
    const name = [disk.vendor, disk.model].filter((part) => part !== '').join(' ')
    return name === '' ? t('disk.unnamed') : name
  }

  /** 不能浏览的原因；文件系统认得出、却打不开时按「参数不对」说 */
  function unbrowsableReason(filesystem: string): string {
    const key = `partition.picker.reason.${filesystem}`
    return te(key) ? t(key) : t('partition.picker.reason.invalid')
  }

  /** 分区的名字：卷标，没有时用 GPT 分区名；都没有时为空串 */
  function partitionLabel(partition: { label: string | null; name?: string | null }): string {
    return [partition.label, partition.name].find((text) => typeof text === 'string' && text !== '') ?? ''
  }

  /** 「1 号分区 卷标（NTFS，8.0 MiB）」：没有名字时不留空位 */
  function partitionLine(partition: {
    number: number
    label: string | null
    name?: string | null
    filesystem: string
    length: number
  }): string {
    const label = partitionLabel(partition)
    return t('partition.line', {
      number: partition.number,
      label: label === '' ? '' : ` ${label}`,
      filesystem: filesystemName(partition.filesystem),
      size: formatBytes(partition.length),
    })
  }

  /** 源上的一段：分区按 partitionLine，分区以外的空隙说「未分区的空间」与大小（制作镜像选范围用） */
  function regionLine(
    region: { partition: number | null; length: number },
    partitions: Parameters<typeof partitionLine>[0][],
  ): string {
    const partition = partitions.find((candidate) => candidate.number === region.partition)
    return partition === undefined
      ? t('partition.gap', { size: formatBytes(region.length) })
      : partitionLine(partition)
  }

  /** 引导扇区字段（规范里的名字）的显示名；修复打不开的分区比对主备、检查文件系统报参数问题共用 */
  function bootFieldName(field: string): string {
    const key = `bootField.${field}`
    return te(key) ? t(key) : field
  }

  /** 引导扇区字段的值：序列号、介质描述符、标志按十六进制，其余按本地数字格式 */
  function bootFieldValue(field: string, value: number): string {
    return HEX_BOOT_FIELDS.includes(field) ? formatHex(value, 2) : value.toLocaleString(locale.value)
  }

  return {
    filesystemName,
    roleName,
    tableNote,
    sourceName,
    diskName,
    unbrowsableReason,
    partitionLabel,
    partitionLine,
    regionLine,
    bootFieldName,
    bootFieldValue,
  }
}
