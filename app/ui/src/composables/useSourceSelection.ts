import { computed, ref, watch } from 'vue'
import { prepareBroker, type JobTarget } from '../services/host'

/** 选源的方式：镜像文件、物理磁盘或其中一个卷（盘符）、某个文件夹（读它所在的卷，找回里面的文件） */
export type SourceKind = 'image' | 'disk' | 'folder'

/** 磁盘列表里选中的一项：整块物理磁盘，或其中一个卷 */
export type DeviceTarget = Extract<JobTarget, { kind: 'disk' | 'volume' }>

/** 文件夹的位置：所在卷的盘符，以及在卷里的各级文件夹名（卷的根目录为空） */
export interface FolderLocation {
  letter: string
  path: string[]
}

/** 「D:\照片\2024」这样的完整路径拆成盘符与各级名字；不是「盘符:\…」的形式（如网络路径）时为 null */
export function parseFolder(text: string): FolderLocation | null {
  const match = /^([A-Za-z]):(?:[\\/](.*))?$/.exec(text.trim())
  if (match === null) {
    return null
  }
  const [, letter, rest = ''] = match
  return {
    letter: letter.toUpperCase(),
    path: rest.split(/[\\/]+/).filter((name) => name !== ''),
  }
}

/** 本次运行里提前提权过了（不论用户同意还是拒绝）：只在第一次选中磁盘或卷时弹 UAC，拒绝了也不追着弹，开始任务时照常会问 */
let hasPrepared = false

/** 选中物理磁盘或卷时先请宿主提权启动磁盘代理，等用户点开始时 UAC 已经答完 */
function prepareDeviceAccess() {
  if (hasPrepared) {
    return
  }
  hasPrepared = true
  prepareBroker().catch(() => {
    // 拒绝或启动失败都不在这里提示：开始任务时会再问一次，并按那时的结果报错
  })
}

/**
 * 来源选择的状态与选出的源，配合 SourcePicker 使用。全部向导共用：选镜像文件时是镜像，选磁盘时是整块盘或其中一个卷，
 * 选文件夹时是它所在的卷（`folderLocation` 给出在卷里的位置）
 */
export function useSourceSelection(initialKind: SourceKind = 'image') {
  const sourceKind = ref<SourceKind>(initialKind)
  const imagePath = ref('')
  const device = ref<DeviceTarget | null>(null)
  const folder = ref('')

  const folderLocation = computed(() => parseFolder(folder.value))

  const target = computed<JobTarget | null>(() => {
    switch (sourceKind.value) {
      case 'image': {
        const path = imagePath.value.trim()
        return path === '' ? null : { kind: 'image', path }
      }
      case 'disk':
        return device.value
      case 'folder': {
        const location = folderLocation.value
        return location === null ? null : { kind: 'volume', letter: location.letter }
      }
    }
    return null
  })

  watch(target, (current) => {
    if (current !== null && current.kind !== 'image') {
      prepareDeviceAccess()
    }
  })

  return { sourceKind, imagePath, device, folder, folderLocation, target }
}
