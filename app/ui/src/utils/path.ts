/** 把文件名接到目录后面；目录已带分隔符（\ 或 /）时不重复加 */
export function joinPath(dir: string, name: string): string {
  return /[\\/]$/.test(dir) ? `${dir}${name}` : `${dir}\\${name}`
}
