import { useI18n } from 'vue-i18n'
import type { FileType } from '../services/host'

/** 扩展名属于格式列表里的第几个格式：格式本身的扩展名，或它按内容改用的（ZIP 里是 Word 文档时的 docx）；没有时为 -1 */
export function formatIndex(types: FileType[], extension: string): number {
  return types.findIndex((type) => type.extension === extension || type.variants.includes(extension))
}

export function useFileTypeName() {
  const { t, te } = useI18n()

  /** 一种文件的显示名：按扩展名取 fileTypes.* 文案，取不到时用引擎给的英文简述，再没有就用大写扩展名 */
  function fileTypeName(extension: string, description?: string): string {
    const key = `fileTypes.${extension}`
    if (te(key)) {
      return t(key)
    }
    return description ?? extension.toUpperCase()
  }

  /** 找到的文件的类型名：按它自己的扩展名取文案，取不到时用所属格式的英文简述 */
  function foundTypeName(types: FileType[], extension: string): string {
    return fileTypeName(extension, types[formatIndex(types, extension)]?.description)
  }

  /**
   * 格式列表里一项的名字。一个格式可能存成好几种扩展名（Office 97–2003 文档有 doc、xls、ppt），
   * 先取说整个格式的 fileFormats.* 文案，没有时同 {@link fileTypeName}
   */
  function formatName(type: FileType): string {
    const key = `fileFormats.${type.extension}`
    return te(key) ? t(key) : fileTypeName(type.extension, type.description)
  }

  return { fileTypeName, foundTypeName, formatName }
}
