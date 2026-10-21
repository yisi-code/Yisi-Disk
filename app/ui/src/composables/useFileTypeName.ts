import { useI18n } from 'vue-i18n'

/** 格式的显示名：按扩展名取 fileTypes.* 文案，取不到时用引擎给的英文简述，再没有就用大写扩展名 */
export function useFileTypeName() {
  const { t, te } = useI18n()

  function fileTypeName(extension: string, description?: string): string {
    const key = `fileTypes.${extension}`
    if (te(key)) {
      return t(key)
    }
    return description ?? extension.toUpperCase()
  }

  return { fileTypeName }
}
