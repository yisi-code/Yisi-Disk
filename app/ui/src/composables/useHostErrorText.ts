import { useI18n } from 'vue-i18n'
import type { HostError } from '../services/host'

/** 把宿主错误翻译成当前语言：按 code 取 errors.* 文案，附上技术细节 */
export function useHostErrorText() {
  const { t, te } = useI18n()

  function formatHostError(error: HostError): string {
    const key = `errors.${error.code}`
    const message = te(key) ? t(key) : t('errors.unknown')
    return error.detail === '' ? message : t('errors.withDetail', { message, detail: error.detail })
  }

  return { formatHostError }
}
