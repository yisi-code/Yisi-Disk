const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'] as const

/** 用时的秒数，保留一位小数（文案是 `common.seconds`） */
export function formatSeconds(milliseconds: number): string {
  return (milliseconds / 1000).toFixed(1)
}

/** 本地时间 yyyy-MM-dd HH:mm:ss */
export function formatDateTime(date: Date): string {
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}

/** 十六进制，例如 0x0F；`digits` 是至少几位 */
export function formatHex(value: number, digits = 0): string {
  return `0x${value.toString(16).toUpperCase().padStart(digits, '0')}`
}

/** 按 1024 进位显示字节数，例如 931.5 GiB */
export function formatBytes(bytes: number): string {
  let value = bytes
  let unitIndex = 0
  while (value >= 1024 && unitIndex < BYTE_UNITS.length - 1) {
    value /= 1024
    unitIndex += 1
  }
  return `${value.toFixed(unitIndex === 0 ? 0 : 1)} ${BYTE_UNITS[unitIndex]}`
}
