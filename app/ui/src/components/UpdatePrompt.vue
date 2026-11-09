<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useHostErrorText } from '../composables/useHostErrorText'
import { useUpdateCheck } from '../composables/useUpdateCheck'
import { openReleasesPage, toHostError, type HostError } from '../services/host'

/**
 * 自动检查发现新版本时的提示：「立即更新」交给父级（进入关于页并开始下载安装，进度在那里显示），
 * 「以后再说」或按 Esc 关掉，下次自动检查（一天后）再提示
 */
const props = defineProps<{
  latest: string
  current: string | null
}>()

const emit = defineEmits<{
  update: []
  dismiss: []
}>()

const { t } = useI18n()
const { formatHostError } = useHostErrorText()
const { source } = useUpdateCheck()

const dialog = ref<HTMLDialogElement | null>(null)
const openFailure = ref<HostError | null>(null)
/** 点了「立即更新」关窗时不再当作「以后再说」 */
let accepted = false

onMounted(() => {
  dialog.value?.showModal()
})

function update() {
  accepted = true
  dialog.value?.close()
  emit('update')
}

function handleClose() {
  if (!accepted) {
    emit('dismiss')
  }
}

async function openReleases() {
  openFailure.value = null
  try {
    await openReleasesPage(source.value ?? undefined)
  } catch (error) {
    openFailure.value = toHostError(error)
  }
}
</script>

<template>
  <dialog ref="dialog" class="update-prompt" aria-labelledby="update-prompt-title" @close="handleClose">
    <h2 id="update-prompt-title" class="update-prompt__title">
      {{ t('updatePrompt.title', { version: props.latest }) }}
    </h2>
    <p class="update-prompt__text">
      {{ props.current ? t('updatePrompt.current', { version: props.current }) : '' }}{{ t('updatePrompt.body') }}
    </p>
    <p v-if="openFailure" class="wizard__error" role="alert">{{ formatHostError(openFailure) }}</p>
    <div class="update-prompt__actions">
      <button class="wizard__button wizard__button--secondary" type="button" @click="openReleases">
        {{ t('updatePrompt.releases') }}
      </button>
      <span class="update-prompt__spacer"></span>
      <button class="wizard__button wizard__button--secondary" type="button" @click="dialog?.close()">
        {{ t('updatePrompt.later') }}
      </button>
      <button class="wizard__button" type="button" autofocus @click="update">
        {{ t('updatePrompt.update') }}
      </button>
    </div>
  </dialog>
</template>

<style scoped>
.update-prompt {
  width: min(32rem, calc(100vw - 2 * var(--space-lg)));
  padding: var(--space-lg);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.update-prompt::backdrop {
  background: rgb(0 0 0 / 0.35);
}

.update-prompt__title {
  margin: 0 0 var(--space-sm);
  font-size: var(--font-size-page-title);
  font-weight: 600;
}

.update-prompt__text {
  margin: 0 0 var(--space-lg);
  color: var(--color-text-muted);
}

/* 发布页靠左，以后再说与立即更新靠右 */
.update-prompt__actions {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.update-prompt__spacer {
  flex: 1;
}
</style>
