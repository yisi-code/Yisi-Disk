<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useEngineCapabilities } from '../composables/useEngineCapabilities'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useUpdateCheck } from '../composables/useUpdateCheck'
import { getAppInfo, openReleasesPage, toHostError, type HostError } from '../services/host'

/** 项目主页与作者的联系方式：是事实，不随界面语言变 */
const REPOSITORY = 'https://github.com/yisi-code/Yisi-Disk'
const CONTACTS = [
  { key: 'developer', value: '倚肆' },
  { key: 'wechat', value: 'm1751250104' },
  { key: 'qq', value: '1751250104' },
  { key: 'email', value: 'xiejing0520@petalmail.com' },
] as const

const { t } = useI18n()
const { formatHostError } = useHostErrorText()

const appVersion = ref<string | null>(null)
const appError = ref<HostError | null>(null)
const { capabilities, failure: engineError, load: loadEngine } = useEngineCapabilities()
const { state: update, check: checkUpdate, install: installUpdate } = useUpdateCheck()
const openFailure = ref<HostError | null>(null)

async function openReleases() {
  openFailure.value = null
  try {
    await openReleasesPage()
  } catch (error) {
    openFailure.value = toHostError(error)
  }
}

// 每次打开都现查一次：引擎是否可用以这一刻为准（各功能卡片也跟着更新）
onMounted(() => {
  void loadAppVersion()
  void loadEngine(true)
})

async function loadAppVersion() {
  try {
    appVersion.value = (await getAppInfo()).version
  } catch (error) {
    appError.value = toHostError(error)
  }
}
</script>

<template>
  <section class="guide-page" aria-labelledby="about-title">
    <header class="guide-page__header">
      <h2 id="about-title" class="guide-page__title">{{ t('about.title') }}</h2>
      <p class="guide-page__intro">{{ t('about.intro') }}</p>
    </header>

    <dl class="about__facts">
      <dt>{{ t('about.version') }}</dt>
      <dd>
        <span v-if="appError" class="wizard__error" role="alert">{{ formatHostError(appError) }}</span>
        <template v-else>{{ appVersion ?? t('about.checking') }}</template>
        <!-- 只在点按时由宿主联网：查最新版本；有新版本时下载、核对签名后换上并重启，也可以去下载页手动下载 -->
        <span v-if="appVersion" class="about__update" aria-live="polite">
          <button
            v-if="update.kind !== 'installing' && update.kind !== 'restarting'"
            class="wizard__button wizard__button--secondary"
            type="button"
            :disabled="update.kind === 'checking'"
            @click="checkUpdate"
          >
            {{ update.kind === 'checking' ? t('about.update.checking') : t('about.update.check') }}
          </button>
          <span v-if="update.kind === 'latest'">{{ t('about.update.latest', { version: update.latest }) }}</span>
          <template v-else-if="update.kind === 'available'">
            <span>{{ t('about.update.available', { version: update.latest }) }}</span>
            <button class="wizard__button" type="button" @click="installUpdate(update.latest)">
              {{ t('about.update.install') }}
            </button>
          </template>
          <span v-else-if="update.kind === 'installing'">
            {{ t('about.update.installing', { version: update.latest }) }}
          </span>
          <span v-else-if="update.kind === 'restarting'">
            {{ t('about.update.restarting', { version: update.version }) }}
          </span>
          <span v-else-if="update.kind === 'failed'" class="wizard__error" role="alert">
            {{ formatHostError(update.error) }}
          </span>
          <button
            v-if="update.kind === 'available' || (update.kind === 'failed' && update.latest !== null)"
            class="wizard__button wizard__button--secondary"
            type="button"
            @click="openReleases"
          >
            {{ t('about.update.open') }}
          </button>
          <span v-if="openFailure" class="wizard__error" role="alert">{{ formatHostError(openFailure) }}</span>
        </span>
        <span class="about__hint">{{ t('about.update.privacy') }}</span>
      </dd>
      <dt>{{ t('about.engine') }}</dt>
      <dd aria-live="polite">
        <template v-if="capabilities">{{ t('about.engineReady') }}</template>
        <template v-else-if="engineError">
          <span class="wizard__error" role="alert">{{ t('about.engineFailed') }}</span>
          <span class="about__hint">{{ formatHostError(engineError) }}</span>
        </template>
        <template v-else>{{ t('about.checking') }}</template>
      </dd>
    </dl>

    <dl class="about__facts">
      <dt>{{ t('about.techStack') }}</dt>
      <dd>{{ t('about.techStackValue') }}</dd>
      <dt>{{ t('about.repository') }}</dt>
      <dd class="wizard__path-text">{{ REPOSITORY }}</dd>
      <template v-for="contact in CONTACTS" :key="contact.key">
        <dt>{{ t(`about.contact.${contact.key}`) }}</dt>
        <dd class="wizard__path-text">{{ contact.value }}</dd>
      </template>
    </dl>

    <p class="about__note">{{ t('about.feedback') }}</p>
    <p class="about__note">{{ t('about.license') }}</p>

    <details v-if="capabilities" class="about__details">
      <summary class="about__summary">{{ t('about.technical') }}</summary>
      <dl class="about__facts">
        <dt>{{ t('about.engineVersion') }}</dt>
        <dd>{{ capabilities.version }}</dd>
        <dt>{{ t('about.engineMethods') }}</dt>
        <dd>
          <ul class="about__methods">
            <li v-for="method in capabilities.methods" :key="method">
              <code>{{ method }}</code>
            </li>
          </ul>
        </dd>
      </dl>
    </details>
  </section>
</template>

<style scoped>
.about__facts {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: var(--space-xs) var(--space-md);
  margin: 0;
}

.about__facts dt {
  color: var(--color-text-muted);
}

.about__facts dd {
  display: flex;
  flex-direction: column;
  margin: 0;
}

.about__note {
  margin: 0;
}

.about__update {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-sm);
  margin-top: var(--space-xs);
}

.about__hint {
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  overflow-wrap: anywhere;
}

.about__details {
  padding: var(--space-sm) var(--space-md);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.about__summary {
  cursor: pointer;
}

.about__details[open] .about__summary {
  margin-bottom: var(--space-sm);
}

.about__methods {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-xs) var(--space-md);
  margin: 0;
  padding: 0;
  list-style: none;
}

.about__methods code {
  font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
  font-size: var(--font-size-small);
}
</style>
