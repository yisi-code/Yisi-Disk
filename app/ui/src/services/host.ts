// 与宿主通信的唯一入口：组件不直接调用 Tauri 命令
import { invoke } from '@tauri-apps/api/core'

export interface AppInfo {
  version: string
}

export function getAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>('app_info')
}
