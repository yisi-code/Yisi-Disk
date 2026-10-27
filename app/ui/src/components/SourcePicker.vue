<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import DiskPicker from './DiskPicker.vue'
import { useHostErrorText } from '../composables/useHostErrorText'
import type { DeviceTarget, SourceKind } from '../composables/useSourceSelection'
import { pickDirectory, pickFile, toHostError, type HostError } from '../services/host'

/**
 * 选来源：镜像文件、物理磁盘（整块盘或其中一个卷），给了 folderHint 时还可以选文件夹。状态由 useSourceSelection 提供。
 * diskHint 显示在磁盘列表下方，imageHint、folderHint 显示在路径下方，各向导自己给
 */
const props = defineProps<{ diskHint: string; imageHint?: string; folderHint?: string }>()
const sourceKind = defineModel<SourceKind>('kind', { required: true })
const imagePath = defineModel<string>('imagePath', { required: true })
const device = defineModel<DeviceTarget | null>('device', { required: true })
const folder = defineModel<string>('folder', { default: '' })

const { t } = useI18n()
const { formatHostError } = useHostErrorText()

const dialogError = ref<HostError | null>(null)

/** 系统的选择文件对话框；用户取消时什么也不改 */
async function browseImage() {
  dialogError.value = null
  try {
    const path = await pickFile(t('source.pickTitle'))
    if (path !== null) {
      imagePath.value = path
    }
  } catch (error) {
    dialogError.value = toHostError(error)
  }
}

/** 系统的选择文件夹对话框；用户取消时什么也不改 */
async function browseFolder() {
  dialogError.value = null
  try {
    const path = await pickDirectory(t('source.pickFolderTitle'))
    if (path !== null) {
      folder.value = path
    }
  } catch (error) {
    dialogError.value = toHostError(error)
  }
}
</script>

<template>
  <div class="source-picker">
    <fieldset class="source-picker__choice">
      <legend class="source-picker__legend">{{ t('source.legend') }}</legend>
      <label class="source-picker__option">
        <input v-model="sourceKind" type="radio" name="source-kind" value="image" />
        {{ t('source.image') }}
      </label>
      <label class="source-picker__option">
        <input v-model="sourceKind" type="radio" name="source-kind" value="disk" />
        {{ t('source.disk') }}
      </label>
      <label v-if="props.folderHint" class="source-picker__option">
        <input v-model="sourceKind" type="radio" name="source-kind" value="folder" />
        {{ t('source.folder') }}
      </label>
    </fieldset>

    <div v-if="sourceKind === 'image'" class="source-picker__path">
      <label class="source-picker__label" for="source-image-path">{{ t('source.imagePath') }}</label>
      <input
        id="source-image-path"
        v-model="imagePath"
        class="source-picker__input"
        type="text"
        spellcheck="false"
        autocomplete="off"
        :placeholder="t('source.imagePathPlaceholder')"
      />
      <button class="source-picker__button" type="button" @click="browseImage">
        {{ t('source.browse') }}
      </button>
    </div>
    <p v-if="sourceKind === 'image' && imageHint" class="source-picker__hint">{{ imageHint }}</p>
    <template v-else-if="sourceKind === 'disk'">
      <DiskPicker v-model="device" :disabled="false" />
      <p class="source-picker__hint">{{ diskHint }}</p>
    </template>
    <template v-else-if="sourceKind === 'folder'">
      <div class="source-picker__path">
        <label class="source-picker__label" for="source-folder-path">{{ t('source.folderPath') }}</label>
        <input
          id="source-folder-path"
          v-model="folder"
          class="source-picker__input"
          type="text"
          spellcheck="false"
          autocomplete="off"
          :placeholder="t('source.folderPathPlaceholder')"
        />
        <button class="source-picker__button" type="button" @click="browseFolder">
          {{ t('common.browseFolder') }}
        </button>
      </div>
      <p class="source-picker__hint">{{ folderHint }}</p>
    </template>

    <p v-if="dialogError" class="source-picker__error" role="alert">
      {{ formatHostError(dialogError) }}
    </p>
  </div>
</template>

<style scoped>
.source-picker {
  display: flex;
  flex-direction: column;
  gap: var(--space-sm);
  width: 100%;
}

.source-picker__choice {
  display: flex;
  gap: var(--space-md);
  margin: 0;
  padding: 0;
  border: none;
}

.source-picker__legend {
  float: left;
  margin-right: var(--space-sm);
  padding: 0;
}

.source-picker__option {
  display: flex;
  align-items: center;
  gap: var(--space-xs);
  cursor: pointer;
}

.source-picker__path {
  display: flex;
  align-items: center;
  gap: var(--space-sm);
}

.source-picker__label {
  flex: none;
}

.source-picker__input {
  flex: 1;
  min-width: 0;
  padding: var(--space-xs) var(--space-sm);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  font: inherit;
}

.source-picker__button {
  flex: none;
  padding: var(--space-xs) var(--space-md);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-control);
  font: inherit;
  cursor: pointer;
}

.source-picker__hint {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-small);
}

.source-picker__error {
  margin: 0;
  color: var(--color-danger);
}
</style>
