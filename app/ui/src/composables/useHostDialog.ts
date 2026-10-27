import { ref } from 'vue'
import { pickDirectory, toHostError, type HostError } from '../services/host'

/**
 * 系统对话框的出错状态与选文件夹：用户取消时得到 null；对话框打不开时把原因记在 `dialogError` 里，页面显示它。
 * 深度恢复、制作镜像与分区文件另存共用
 */
export function useHostDialog() {
  const dialogError = ref<HostError | null>(null)

  async function pickFolder(title: string): Promise<string | null> {
    dialogError.value = null
    try {
      return await pickDirectory(title)
    } catch (error) {
      dialogError.value = toHostError(error)
      return null
    }
  }

  return { dialogError, pickFolder }
}
