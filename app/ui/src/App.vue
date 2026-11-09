<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AboutPanel from './components/AboutPanel.vue'
import BadSectorCheck from './components/BadSectorCheck.vue'
import CarveWizard from './components/CarveWizard.vue'
import DiskImage from './components/DiskImage.vue'
import DiskAccessNotice from './components/DiskAccessNotice.vue'
import DiskSurvey from './components/DiskSurvey.vue'
import EngineStatus from './components/EngineStatus.vue'
import UpdatePrompt from './components/UpdatePrompt.vue'
import { useUpdateCheck } from './composables/useUpdateCheck'
import FeatureCatalog from './components/FeatureCatalog.vue'
import FindPartitionsWizard from './components/FindPartitionsWizard.vue'
import FsCheckWizard from './components/FsCheckWizard.vue'
import ProblemGuide from './components/ProblemGuide.vue'
import ProblemOverview from './components/ProblemOverview.vue'
import RepairPartitionWizard from './components/RepairPartitionWizard.vue'
import SettingsPanel from './components/SettingsPanel.vue'
import UndeleteWizard from './components/UndeleteWizard.vue'
import { acceptFeatureJump } from './composables/useFeatureJump'
import {
  FEATURE_VIEWS,
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
/** 自动检查到的新版本，等用户回到导航页时弹出提示；关掉后为 null */
const promptedVersion = ref<string | null>(null)
const { autoCheck, install: installUpdate } = useUpdateCheck()

/** 打开后等一会儿再查，不和启动抢资源 */
const AUTO_CHECK_DELAY_MS = 5000

watch(
  locale,
  (currentLocale) => {
    document.documentElement.lang = currentLocale
  },
  { immediate: true },
)

onMounted(async () => {
  window.setTimeout(() => {
    void autoCheck().then((latest) => {
      promptedVersion.value = latest
    })
  }, AUTO_CHECK_DELAY_MS)
  try {
    appVersion.value = (await getAppInfo()).version
  } catch (error) {
    hostError.value = String(error)
  }
})

/** 弹窗里点「立即更新」：到关于页，开始下载安装，进度在关于页的更新区显示 */
function updateNow() {
  const latest = promptedVersion.value
  promptedVersion.value = null
  if (latest === null) {
    return
  }
  page.value = 'about'
  void installUpdate(latest)
}

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

/**
 * 从功能页返回：回到原来的导航页，焦点回到进入时点的那张卡片。
 * 功能页是为了转到另一个功能才退出的，就直接打开那个功能；之后从那里返回时焦点落在导航页标题上
 */
function closeFeature() {
  const next = acceptFeatureJump()
  const nextView = next === null ? null : FEATURE_VIEWS[next]
  if (nextView !== null) {
    openedFeature.value = null
    featureView.value = nextView
    return
  }
  const feature = openedFeature.value
  focusRequest.value = feature === null ? { target: 'title' } : { target: 'feature', feature }
  featureView.value = null
}
</script>

<template>
  <div class="app-layout">
    <!-- 程序名在窗口标题栏与「关于」里；顶部不再占一行，内容区（文件浏览）多些高度 -->
    <h1 class="wizard__visually-hidden">{{ t('app.name') }}</h1>

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
          <li v-for="other in ['allFeatures', 'settings', 'about'] as const" :key="other">
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
        <DiskSurvey v-if="featureView === 'survey'" @exit="closeFeature" />
        <CarveWizard v-else-if="featureView === 'carve'" @exit="closeFeature" />
        <UndeleteWizard v-else-if="featureView === 'undelete'" @exit="closeFeature" />
        <BadSectorCheck v-else-if="featureView === 'badSectors'" @exit="closeFeature" />
        <DiskImage v-else-if="featureView === 'image'" @exit="closeFeature" />
        <FindPartitionsWizard v-else-if="featureView === 'findPartitions'" @exit="closeFeature" />
        <RepairPartitionWizard v-else-if="featureView === 'repairPartition'" @exit="closeFeature" />
        <FsCheckWizard v-else-if="featureView === 'fsCheck'" @exit="closeFeature" />
        <ProblemOverview
          v-else-if="page === 'overview'"
          :focus-request="focusRequest"
          @navigate="navigateFromOverview"
          @open="openFeature"
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
        <SettingsPanel v-else-if="page === 'settings'" />
        <AboutPanel v-else />
      </main>
    </div>

    <!-- 功能页里可能有任务在跑，不打断：回到导航页时再提示 -->
    <UpdatePrompt
      v-if="promptedVersion !== null && featureView === null"
      :latest="promptedVersion"
      :current="appVersion"
      @update="updateNow"
      @dismiss="promptedVersion = null"
    />

    <footer class="app-footer">
      <EngineStatus />
      <DiskAccessNotice />
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

/* 纵向排列，向导的结果页能铺满剩下的高度（wizard.css 的 .wizard--fill）。
   相对定位：里面绝对定位的元素（例如只给读屏软件读的文字）以内容区为参照、随内容区滚动，不会撑高整页 */
.app-main {
  position: relative;
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  overflow: auto;
  /* 上下留得少些，让文件区多占几行 */
  padding: var(--space-md) var(--space-lg);
}

.app-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-md);
  padding: var(--space-sm) var(--space-lg);
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
  background: var(--color-surface);
  border-top: 1px solid var(--color-border);
}
</style>
