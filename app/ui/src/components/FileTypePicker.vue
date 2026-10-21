<script setup lang="ts">
import { useI18n } from 'vue-i18n'
import { useFileTypeName } from '../composables/useFileTypeName'
import type { FileType } from '../services/host'

defineProps<{ types: FileType[]; disabled: boolean }>()
const selectedExtensions = defineModel<string[]>({ required: true })

const { t } = useI18n()
const { fileTypeName } = useFileTypeName()
</script>

<template>
  <fieldset class="file-type-picker" :disabled="disabled">
    <legend class="file-type-picker__legend">{{ t('carve.formats.legend') }}</legend>
    <ul class="file-type-picker__list">
      <li v-for="type in types" :key="type.extension">
        <label class="file-type-picker__option">
          <input
            v-model="selectedExtensions"
            class="file-type-picker__checkbox"
            type="checkbox"
            :value="type.extension"
          />
          <span class="file-type-picker__name">
            {{ fileTypeName(type.extension, type.description) }}
          </span>
          <span class="file-type-picker__extension">.{{ type.extension }}</span>
        </label>
      </li>
    </ul>
  </fieldset>
</template>

<style scoped>
.file-type-picker {
  display: block;
  width: 100%;
  margin: 0;
  padding: var(--space-sm) var(--space-md);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-card);
}

.file-type-picker__legend {
  padding: 0 var(--space-xs);
  color: var(--color-text-muted);
}

.file-type-picker__list {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: var(--space-xs) var(--space-md);
  margin: 0;
  padding: 0;
  list-style: none;
}

.file-type-picker__option {
  display: flex;
  align-items: baseline;
  gap: var(--space-sm);
  padding: var(--space-xs) 0;
  cursor: pointer;
}

.file-type-picker__checkbox {
  align-self: center;
  accent-color: var(--color-accent);
}

.file-type-picker__name {
  font-weight: 600;
}

.file-type-picker__extension {
  color: var(--color-text-muted);
}
</style>
