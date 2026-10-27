<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AboutPanel from './components/AboutPanel.vue'
import BadSectorCheck from './components/BadSectorCheck.vue'
import CarveWizard from './components/CarveWizard.vue'
import DiskImage from './components/DiskImage.vue'
import FeatureCatalog from './components/FeatureCatalog.vue'
import ProblemGuide from './components/ProblemGuide.vue'
import ProblemOverview from './components/ProblemOverview.vue'
import RepairPartitionWizard from './components/RepairPartitionWizard.vue'
import UndeleteWizard from './components/UndeleteWizard.vue'
import { SUPPORTED_LOCALES } from './i18n'
import {
  PROBLEMS,
  type FeatureKey,
  type FeatureView,
  type FocusRequest,
  type NavPage,
  type ProblemKey,
} from './navigation'
import { getAppInfo } from './services/host'

const PROBLEM_KEYS: ProblemKey[] = PROBLEMS.map((problem) => problem.key)

const { t, locale } = useI18n()

/** 当前导航页；进入功能页时导航栏收起，返回时回到这一页 */
const page = ref<NavPage>('overview')
/** 打开的功能页；为 null 时显示导航页 */
const featureView = ref<FeatureView | null>(null)
/** 进入功能页前点的卡片，返回时焦点回到它上面 */
const openedFeature = ref<FeatureKey | null>(null)
const focusRequest = ref<FocusRequest | null>(null)
const appVersion = ref<string | null>(null)
const hostError = ref<string | null>(null)

watch(
  locale,
  (currentLocale) => {
    document.documentElement.lang = currentLocale
  },
  { immediate: true },
)

onMounted(async () => {
  try {
    appVersion.value = (await getAppInfo()).version
  } catch (error) {
    hostError.value = String(error)
  }
})

function isProblemPage(candidate: NavPage): candidate is ProblemKey {
  return (PROBLEM_KEYS as NavPage[]).includes(candidate)
}

/** 导航栏切换：焦点留在导航项上，便于继续浏览 */
function selectPage(next: NavPage) {
  focusRequest.value = null
  page.value = next
}

/** 首页上点问题卡片：卡片随页面消失，焦点移到新页面的标题 */
function navigateFromOverview(next: NavPage) {
  focusRequest.value = { target: 'title' }
  page.value = next
}

function openFeature(view: FeatureView, feature: FeatureKey) {
  openedFeature.value = feature
  featureView.value = view
}

/** 从功能页返回：回到原来的导航页，焦点回到进入时点的那张卡片 */
function closeFeature() {
  const feature = openedFeature.value
  focusRequest.value = feature === null ? { target: 'title' } : { target: 'feature', feature }
  featureView.value = null
}
</script>

<template>
  <div class="app-layout">
    <header class="app-header">
      <div class="app-header__brand">
        <h1 class="app-header__title">{{ t('app.name') }}</h1>
        <span class="app-header__subtitle">{{ t('app.subtitle') }}</span>
      </div>
      <label class="locale-picker">
        <span class="locale-picker__label">{{ t('locale.label') }}</span>
        <select v-model="locale" class="locale-picker__select">
          <option v-for="code in SUPPORTED_LOCALES" :key="code" :value="code">
            {{ t(`locale.${code}`) }}
          </option>
        </select>
      </label>
    </header>

    <div class="app-body">
      <!-- 功能页运行中不能离开，导航栏随功能页一起收起 -->
      <nav v-if="featureView === null" class="app-nav" :aria-label="t('nav.label')">
        <ul class="app-nav__list">
          <li>
            <button
              class="app-nav__item"
              :class="{ 'app-nav__item--current': page === 'overview' }"
              type="button"
              :aria-current="page === 'overview' ? 'page' : undefined"
              @click="selectPage('overview')"
            >
              {{ t('nav.overview') }}
            </button>
          </li>
        </ul>
        <h2 id="app-nav-problems" class="app-nav__group">{{ t('nav.problems') }}</h2>
        <ul class="app-nav__list" aria-labelledby="app-nav-problems">
          <li v-for="problem in PROBLEM_KEYS" :key="problem">
            <button
              class="app-nav__item"
              :class="{ 'app-nav__item--current': page === problem }"
              type="button"
              :aria-current="page === problem ? 'page' : undefined"
              @click="selectPage(problem)"
            >
              {{ t(`problems.${problem}.name`) }}
            </button>
          </li>
        </ul>
        <ul class="app-nav__list app-nav__list--more">
          <li v-for="other in ['allFeatures', 'about'] as const" :key="other">
            <button
              class="app-nav__item"
              :class="{ 'app-nav__item--current': page === other }"
              type="button"
              :aria-current="page === other ? 'page' : undefined"
              @click="selectPage(other)"
            >
              {{ t(`nav.${other}`) }}
            </button>
          </li>
        </ul>
      </nav>

      <main class="app-main">
        <CarveWizard v-if="featureView === 'carve'" @exit="closeFeature" />
        <UndeleteWizard v-else-if="featureView === 'undelete'" @exit="closeFeature" />
        <BadSectorCheck v-else-if="featureView === 'badSectors'" @exit="closeFeature" />
        <DiskImage v-else-if="featureView === 'image'" @exit="closeFeature" />
        <RepairPartitionWizard v-else-if="featureView === 'repairPartition'" @exit="closeFeature" />
        <ProblemOverview
          v-else-if="page === 'overview'"
          :focus-request="focusRequest"
          @navigate="navigateFromOverview"
        />
        <ProblemGuide
          v-else-if="isProblemPage(page)"
          :key="page"
          :problem="page"
          :focus-request="focusRequest"
          @open="openFeature"
        />
        <FeatureCatalog
          v-else-if="page === 'allFeatures'"
          :focus-request="focusRequest"
          @open="openFeature"
        />
        <AboutPanel v-else />
      </main>
    </div>

    <footer class="app-footer">
      <span v-if="hostError" role="alert">{{ t('host.failed', { reason: hostError }) }}</span>
      <span v-else-if="appVersion">{{ t('host.version', { version: appVersion }) }}</span>
      <span v-else>{{ t('host.connecting') }}</span>
    </footer>
  </div>
</template>

<style scoped>
.app-layout {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.app-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--space-md) var(--space-lg);
  background: var(--color-surface);
  border-bottom: 1px solid var(--color-border);
}

.app-header__brand {
  display: flex;
  align-items: baseline;
  gap: var(--space-sm);
}

.app-header__title {
  margin: 0;
  font-size: var(--font-size-app-title);
  font-weight: 600;
}

.app-header__subtitle {
  color: var(--color-text-muted);
}

.locale-picker {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
  color: var(--color-text-muted);
}

.locale-picker__select {
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--space-xs);
  font: inherit;
}

.app-body {
  display: flex;
  flex: 1;
  min-height: 0;
}

.app-nav {
  display: flex;
  flex: none;
  flex-direction: column;
  gap: var(--space-xs);
  width: 232px;
  overflow: auto;
  padding: var(--space-md) var(--space-sm);
  background: var(--color-surface);
  border-right: 1px solid var(--color-border);
}

.app-nav__list {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.app-nav__list--more {
  margin-top: var(--space-sm);
  padding-top: var(--space-sm);
  border-top: 1px solid var(--color-border);
}

.app-nav__group {
  margin: var(--space-sm) 0 0;
  padding: 0 var(--space-sm);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  font-weight: normal;
}

.app-nav__item {
  display: block;
  width: 100%;
  padding: var(--space-sm);
  color: var(--color-text);
  font: inherit;
  text-align: left;
  background: transparent;
  border: none;
  border-left: 3px solid transparent;
  border-radius: var(--space-xs);
  cursor: pointer;
}

.app-nav__item:hover {
  background: var(--color-bg);
}

/* 当前项除了底色，还有左侧色条与加粗，不只靠颜色区分 */
.app-nav__item--current {
  font-weight: 600;
  background: var(--color-bg);
  border-left-color: var(--color-accent);
}

/* 纵向排列，向导的结果页能铺满剩下的高度（wizard.css 的 .wizard--fill） */
.app-main {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  overflow: auto;
  padding: var(--space-lg);
}

.app-footer {
  padding: var(--space-sm) var(--space-lg);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  background: var(--color-surface);
  border-top: 1px solid var(--color-border);
}
</style>
