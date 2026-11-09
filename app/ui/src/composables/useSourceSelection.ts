import { computed, ref, watch } from 'vue'
import type { JobTarget } from '../services/host'

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

/** 本次运行里最近选过的源：换一个功能时默认还选它（不存盘，程序关掉就忘） */
interface RememberedSource {
  kind: SourceKind
  imagePath: string
  device: DeviceTarget | null
  folder: string
}

let remembered: RememberedSource | null = null

/** 把一个源记成「最近选过的」：从一个功能跳到另一个功能、做完镜像要在镜像上恢复时用 */
export function rememberSource(target: JobTarget) {
  const previous = remembered ?? { kind: 'image', imagePath: '', device: null, folder: '' }
  remembered =
    target.kind === 'image'
      ? { ...previous, kind: 'image', imagePath: target.path }
      : { ...previous, kind: 'disk', device: target }
}

/**
 * 来源选择的状态与选出的源，配合 SourcePicker 使用。全部向导共用：选镜像文件时是镜像，选磁盘时是整块盘或其中一个卷，
 * 选文件夹时是它所在的卷（`folderLocation` 给出在卷里的位置）。
 * 打开时默认选着本次运行里最近选过的源；`initialKind` 是没选过时的默认方式，给了 'disk' 的（制作镜像）不默认选镜像文件。
 * 不能选文件夹的向导（没给 `allowsFolder`），最近选的是文件夹时改选它所在的盘符
 */
export function useSourceSelection(
  initialKind: SourceKind = 'image',
  options: { allowsFolder?: boolean } = {},
) {
  const start = startingSelection(initialKind, options.allowsFolder === true)
  const sourceKind = ref<SourceKind>(start.kind)
  const imagePath = ref(start.imagePath)
  const device = ref<DeviceTarget | null>(start.device)
  const folder = ref(start.folder)

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

  // 选出了一个源就记下来（选到一半、还没选全的不记）
  watch(target, (current) => {
    if (current !== null) {
      remembered = {
        kind: sourceKind.value,
        imagePath: imagePath.value,
        device: device.value,
        folder: folder.value,
      }
    }
  })

  return { sourceKind, imagePath, device, folder, folderLocation, target }
}

function startingSelection(initialKind: SourceKind, allowsFolder: boolean): RememberedSource {
  const last = remembered
  if (last === null) {
    return { kind: initialKind, imagePath: '', device: null, folder: '' }
  }
  let kind = last.kind
  let device = last.device
  if (kind === 'folder' && !allowsFolder) {
    const location = parseFolder(last.folder)
    kind = 'disk'
    device = location === null ? last.device : { kind: 'volume', letter: location.letter }
  }
  if (kind === 'image' && initialKind === 'disk') {
    kind = 'disk'
  }
  return { kind, imagePath: last.imagePath, device, folder: last.folder }
}
