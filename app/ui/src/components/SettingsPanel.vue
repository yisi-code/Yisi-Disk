<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useDiskAccess } from '../composables/useDiskAccess'
import { useHostErrorText } from '../composables/useHostErrorText'
import { SUPPORTED_LOCALES, saveLocale, type AppLocale } from '../i18n'
import {
  listAppFolders,
  openAppFolder,
  toHostError,
  type AppFolder,
  type AppFolderKind,
  type HostError,
} from '../services/host'

/** 设置：界面语言（选了就记住）、磁盘访问的授权、程序文件夹的位置与打开。以后其他全程序的设置也放这里 */
const { t, locale } = useI18n()
const { formatHostError } = useHostErrorText()
const {
  access: diskAccess,
  authorizing,
  failure: authorizeFailure,
  authorize,
} = useDiskAccess()

const folders = ref<AppFolder[] | null>(null)
const foldersError = ref<HostError | null>(null)
/** 打开失败的那个文件夹与原因，显示在它自己那一行 */
const openFailure = ref<{ kind: AppFolderKind; error: HostError } | null>(null)

// 每次进设置页都现查：从没写过盘时备份文件夹还不存在，写过之后再来就能打开了
onMounted(() => {
  void loadFolders()
})

function changeLocale(event: Event) {
  const next = (event.target as HTMLSelectElement).value as AppLocale
  locale.value = next
  saveLocale(next)
}

async function loadFolders() {
  try {
    folders.value = await listAppFolders()
  } catch (error) {
    foldersError.value = toHostError(error)
  }
}

async function openFolder(kind: AppFolderKind) {
  openFailure.value = null
  try {
    await openAppFolder(kind)
  } catch (error) {
    openFailure.value = { kind, error: toHostError(error) }
  }
}
</script>

<template>
  <section class="guide-page" aria-labelledby="settings-title">
    <header class="guide-page__header">
      <h2 id="settings-title" class="guide-page__title">{{ t('settings.title') }}</h2>
      <p class="guide-page__intro">{{ t('settings.intro') }}</p>
    </header>

    <!-- 三块共用一个网格：标签列按全页最长的那个定宽，各块的值从同一条竖线开始 -->
    <div class="settings">
      <dl class="settings__list">
        <dt>
          <label for="settings-locale">{{ t('locale.label') }}</label>
        </dt>
        <dd>
          <select id="settings-locale" class="settings__select" :value="locale" @change="changeLocale">
            <option v-for="code in SUPPORTED_LOCALES" :key="code" :value="code">
              {{ t(`locale.${code}`) }}
            </option>
          </select>
          <p class="settings__hint">{{ t('settings.localeHint') }}</p>
        </dd>
      </dl>

      <section class="settings__section" aria-labelledby="settings-permissions-title">
        <h3 id="settings-permissions-title" class="wizard__section-title">{{ t('settings.permissions.title') }}</h3>
        <p class="settings__hint">{{ t('settings.permissions.hint') }}</p>
        <dl class="settings__list">
          <dt>{{ t('settings.permissions.diskAccess') }}</dt>
          <dd>
            <div class="settings__line" aria-live="polite">
              <span class="settings__value">
                {{ diskAccess === null ? t('settings.folders.loading') : t(`diskAccess.state.${diskAccess}`) }}
              </span>
              <button
                v-if="diskAccess !== null && diskAccess !== 'granted' && diskAccess !== 'unsupported'"
                class="wizard__button wizard__button--secondary"
                type="button"
                :disabled="authorizing"
                @click="authorize"
              >
                {{
                  authorizing
                    ? t('diskAccess.authorizing')
                    : diskAccess === 'notRequested'
                      ? t('diskAccess.authorize')
                      : t('diskAccess.retry')
                }}
              </button>
            </div>
            <p v-if="authorizeFailure" class="wizard__error" role="alert">{{ formatHostError(authorizeFailure) }}</p>
          </dd>
        </dl>
      </section>

      <section class="settings__section" aria-labelledby="settings-folders-title">
        <h3 id="settings-folders-title" class="wizard__section-title">{{ t('settings.folders.title') }}</h3>
        <p class="settings__hint">{{ t('settings.folders.hint') }}</p>
        <p v-if="foldersError" class="wizard__error" role="alert">{{ formatHostError(foldersError) }}</p>
        <p v-else-if="folders === null" class="wizard__note">{{ t('settings.folders.loading') }}</p>
        <dl v-else class="settings__list">
          <template v-for="folder in folders" :key="folder.kind">
            <dt>{{ t(`settings.folders.${folder.kind}`) }}</dt>
            <dd>
              <div class="settings__line">
                <span class="settings__value wizard__path-text">{{ folder.path }}</span>
                <button
                  class="wizard__button wizard__button--secondary"
                  type="button"
                  :disabled="!folder.exists"
                  :aria-label="`${t('settings.folders.open')} ${t(`settings.folders.${folder.kind}`)}`"
                  @click="openFolder(folder.kind)"
                >
                  {{ t('settings.folders.open') }}
                </button>
              </div>
              <p class="settings__hint">
                {{ folder.exists ? t(`settings.folders.${folder.kind}Hint`) : t('settings.folders.missing') }}
              </p>
              <p v-if="openFailure?.kind === folder.kind" class="wizard__error" role="alert">
                {{ formatHostError(openFailure.error) }}
              </p>
            </dd>
          </template>
        </dl>
      </section>
    </div>
  </section>
</template>

<style scoped>
.settings {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: var(--space-lg);
}

/* 各块与块里的清单都沿用外层的两列（subgrid），只管自己的行距 */
.settings__list,
.settings__section {
  display: grid;
  grid-column: 1 / -1;
  grid-template-columns: subgrid;
  row-gap: var(--space-sm);
  margin: 0;
}

.settings__list dt {
  padding-top: var(--space-xs);
  color: var(--color-text-muted);
}

.settings__list dd {
  margin: 0;
}

.settings__select {
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  font: inherit;
}

.settings__hint {
  margin: var(--space-xs) 0 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

/* 块的标题、说明、提示占满两列 */
.settings__section > :not(.settings__list) {
  grid-column: 1 / -1;
}

.settings__section > .settings__hint {
  margin: 0;
}

/* 值在左、按钮在右；值太长时折行，按钮不被挤走 */
.settings__line {
  display: flex;
  align-items: center;
  gap: var(--space-md);
}

.settings__value {
  flex: 1 1 auto;
  min-width: 0;
}
</style>
