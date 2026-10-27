/** 首页按问题导航：有哪些功能、哪些问题、每个问题按什么顺序推荐哪些功能。文案在 features.* 与 problems.* 下 */

/** 能进入的功能页；没有对应页面的功能还在开发中 */
export type FeatureView = 'undelete' | 'carve' | 'badSectors' | 'image' | 'findPartitions'

export type FeatureKey = FeatureView | 'findPartitions' | 'repairPartition' | 'fsCheck'

/** 每个功能进入哪个功能页；null 表示还在开发中 */
export const FEATURE_VIEWS: Record<FeatureKey, FeatureView | null> = {
  undelete: 'undelete',
  carve: 'carve',
  badSectors: 'badSectors',
  image: 'image',
  findPartitions: 'findPartitions',
  repairPartition: null,
  fsCheck: null,
}

/** 「全部功能」页的顺序：能用的在前 */
export const ALL_FEATURES: FeatureKey[] = [
  'undelete',
  'carve',
  'badSectors',
  'image',
  'findPartitions',
  'repairPartition',
  'fsCheck',
]

export type ProblemKey = 'deleted' | 'unopenable' | 'lostPartition' | 'failingDisk'

export interface Problem {
  key: ProblemKey
  /** 按推荐顺序排列；每项在 problems.<key>.steps.<feature> 下有一句针对这个问题的说明 */
  features: FeatureKey[]
}

export const PROBLEMS: Problem[] = [
  { key: 'deleted', features: ['undelete', 'carve'] },
  { key: 'unopenable', features: ['carve', 'repairPartition', 'fsCheck'] },
  { key: 'lostPartition', features: ['findPartitions', 'carve'] },
  { key: 'failingDisk', features: ['badSectors', 'image', 'undelete', 'carve'] },
]

export type NavPage = 'overview' | ProblemKey | 'allFeatures' | 'about'

/** 从别的视图回到导航页时焦点落在哪里：页面标题，或进入功能前点的那张卡片 */
export type FocusRequest = { target: 'title' } | { target: 'feature'; feature: FeatureKey }

