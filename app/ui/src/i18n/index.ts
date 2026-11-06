import { createI18n } from 'vue-i18n'
import zhCN from './locales/zh-CN'
import enUS from './locales/en-US'

type MessageSchema = typeof zhCN

export const SUPPORTED_LOCALES = ['zh-CN', 'en-US'] as const
export type AppLocale = (typeof SUPPORTED_LOCALES)[number]

/** 设置里选的界面语言记在本机（只是方便，读写不了时按系统语言） */
const LOCALE_STORAGE_KEY = 'yisi.locale'

function isAppLocale(value: string | null): value is AppLocale {
  return (SUPPORTED_LOCALES as readonly string[]).includes(value ?? '')
}

function detectLocale(): AppLocale {
  try {
    const saved = localStorage.getItem(LOCALE_STORAGE_KEY)
    if (isAppLocale(saved)) {
      return saved
    }
  } catch {
    // 存储不可用时按系统语言
  }
  return navigator.language.toLowerCase().startsWith('zh') ? 'zh-CN' : 'en-US'
}

/** 记住设置里选的界面语言 */
export function saveLocale(locale: AppLocale) {
  try {
    localStorage.setItem(LOCALE_STORAGE_KEY, locale)
  } catch {
    // 记不住也不影响这次使用
  }
}

export const i18n = createI18n<[MessageSchema], AppLocale, false>({
  legacy: false,
  locale: detectLocale(),
  fallbackLocale: 'en-US',
  messages: {
    'zh-CN': zhCN,
    'en-US': enUS,
  },
})
