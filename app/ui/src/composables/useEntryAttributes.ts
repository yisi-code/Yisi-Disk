import { useI18n } from 'vue-i18n'
import type { FsEntry } from '../services/host'

/** 资源管理器默认不显示、Windows 自己建的文件夹：名字（不分大小写）→ 说明的键 */
const KNOWN_SYSTEM_FOLDERS: Record<string, string> = {
  '$recycle.bin': 'recycleBin',
  'system volume information': 'systemVolumeInformation',
}

/** 分区里文件的隐藏、系统属性怎么写：列表行的标签与详情里的说明共用 */
export function useEntryAttributes() {
  const { t } = useI18n()

  /** 「系统 · 隐藏」「隐藏」「系统」；两个属性都没有时为 null */
  function attributesLabel(entry: FsEntry): string | null {
    const parts = [
      ...(entry.system ? [t('partitionFiles.attributes.system')] : []),
      ...(entry.hidden ? [t('partitionFiles.attributes.hidden')] : []),
    ]
    return parts.length === 0 ? null : parts.join(' · ')
  }

  /** 详情里的说明：资源管理器默认不显示，认得出的系统文件夹另说它是什么 */
  function attributesDetail(entry: FsEntry): string | null {
    const label = attributesLabel(entry)
    if (label === null) {
      return null
    }
    const known = KNOWN_SYSTEM_FOLDERS[entry.name.toLowerCase()]
    const note = known === undefined ? '' : t(`partitionFiles.attributes.known.${known}`)
    return t('partitionFiles.attributes.detail', { label, note })
  }

  return { attributesLabel, attributesDetail }
}
