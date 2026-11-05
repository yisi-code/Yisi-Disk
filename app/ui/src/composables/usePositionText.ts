import { useI18n } from 'vue-i18n'
import { formatBytes } from '../utils/format'

/** 源上一个位置的几种写法，各页面共用 */
export function usePositionText() {
  const { t, locale } = useI18n()

  /** 「第 N 扇区（偏移 X 字节）」：引导扇区诊断、写盘确认页 */
  function sectorPosition(offset: number, sectorSize: number): string {
    return t('common.sectorPosition', {
      sector: Math.floor(offset / sectorSize).toLocaleString(locale.value),
      offset: offset.toLocaleString(locale.value),
    })
  }

  /**
   * 「大小（偏移 X，扇区 N）」：分区里文件的数据位置、深度恢复找到的文件的起止。扇区号不加千位分隔，
   * 与深度恢复起的文件名里的扇区号一致
   */
  function bytePosition(offset: number, sectorSize: number): string {
    return t('common.positionValue', {
      position: formatBytes(offset),
      offset: offset.toLocaleString(locale.value),
      sector: Math.floor(offset / sectorSize),
    })
  }

  /** 「大小（偏移 X 字节）」：读不出的区段（坏道检测、做镜像的结果里没有扇区大小） */
  function rangePosition(offset: number): string {
    return t('common.ranges.positionValue', {
      position: formatBytes(offset),
      offset: offset.toLocaleString(locale.value),
    })
  }

  return { sectorPosition, bytePosition, rangePosition }
}
