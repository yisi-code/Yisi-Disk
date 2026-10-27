import { computed, onBeforeUnmount, ref, watch, type Ref } from 'vue'
import type { ReadingStats } from '../services/host'

/**
 * 读得慢的原因：
 * - damaged：读错过，或者不止一次单次读取超过 2 秒（盘在内部反复重试）——有损坏
 * - stuck：正在读的这一处已经很久没读完，多半碰上了坏扇区
 * - scattered：读得慢但每次只读一点，盘也没出错——内容零散（小文件多、碎片多），是正常的
 * - slowDevice：每次读得不少、也没出错，但盘本身就慢（USB 2、慢速机械盘）
 */
export type ReadHealthKind = 'damaged' | 'stuck' | 'scattered' | 'slowDevice'

export interface ReadHealth {
  kind: ReadHealthKind
  errors: number
  slowReads: number
  stalledSeconds: number
  /** 最近一段时间平均每次读多少字节、每秒读多少字节 */
  averageReadBytes: number
  bytesPerSecond: number
}

/** 读速按最近这么久的统计算，至少要有这么久的样本 */
const WINDOW_MS = 10_000
const MIN_SPAN_MS = 5_000
/** 低于这个读速才算读得慢 */
const SLOW_RATE = 20 * 1024 * 1024
/** 平均每次读得比这少、次数又不少，算内容零散 */
const SMALL_READ = 256 * 1024
const MIN_READS = 20
/** 读取占用了这一段时间的一半以上，慢才是因为盘；否则是在算（识别文件、解析目录），与盘无关 */
const BUSY_SHARE = 0.5
/** 单次读取超过 2 秒的次数到这么多算有损坏：只有一次可能是盘刚从休眠里转起来 */
const SLOW_READS_FOR_DAMAGE = 2
/** 一次读取卡了这么久算碰上了坏扇区 */
const STUCK_MS = 10_000
/** 多久重新判断一次（统计没有变化时引擎不发，靠这个让「最近」往前走） */
const TICK_MS = 1_000

interface Sample {
  at: number
  stats: ReadingStats
}

/**
 * 由任务读源的统计判断读得慢的原因；不慢、或者慢与盘无关时为 null。暂停时只看损坏，不看读速。
 * 全部任务型页面共用，`reading` 来自 useJob
 */
export function useReadHealth(
  reading: Ref<ReadingStats | null>,
  isRunning: Ref<boolean>,
  isPaused: Ref<boolean>,
) {
  const samples = ref<Sample[]>([])
  const now = ref(performance.now())
  let timer: ReturnType<typeof setInterval> | null = null

  watch(reading, (stats) => {
    if (stats === null) {
      samples.value = []
      return
    }
    const at = performance.now()
    now.value = at
    // 留下窗口以内的，再加上紧挨着窗口之前的那一个当起点
    const kept = samples.value.filter((sample) => at - sample.at <= WINDOW_MS)
    const before = samples.value.filter((sample) => at - sample.at > WINDOW_MS).pop()
    samples.value = [...(before ? [before] : []), ...kept, { at, stats }]
  })

  watch(
    isRunning,
    (running) => {
      if (running && timer === null) {
        timer = setInterval(() => {
          now.value = performance.now()
        }, TICK_MS)
      } else if (!running && timer !== null) {
        clearInterval(timer)
        timer = null
      }
    },
    { immediate: true },
  )

  onBeforeUnmount(() => {
    if (timer !== null) {
      clearInterval(timer)
    }
  })

  const health = computed<ReadHealth | null>(() => {
    const latest = samples.value.at(-1)
    if (latest === undefined || !isRunning.value) {
      return null
    }
    const { errors, slowReads } = latest.stats
    // 卡住的时长从收到统计那一刻起继续往上算
    const stalledMs = latest.stats.stalledMs > 0 ? latest.stats.stalledMs + (now.value - latest.at) : 0
    const first = samples.value.find((sample) => now.value - sample.at <= WINDOW_MS + TICK_MS) ?? latest
    const spanMs = now.value - first.at
    const reads = latest.stats.reads - first.stats.reads
    const bytes = latest.stats.bytes - first.stats.bytes
    const busyMs = latest.stats.busyMs - first.stats.busyMs
    const recent = {
      averageReadBytes: reads > 0 ? bytes / reads : 0,
      bytesPerSecond: spanMs > 0 ? (bytes * 1000) / spanMs : 0,
    }
    const base = { errors, slowReads, stalledSeconds: Math.floor(stalledMs / 1000), ...recent }

    if (errors > 0 || slowReads >= SLOW_READS_FOR_DAMAGE) {
      return { kind: 'damaged', ...base }
    }
    if (stalledMs >= STUCK_MS) {
      return { kind: 'stuck', ...base }
    }
    if (isPaused.value || spanMs < MIN_SPAN_MS || recent.bytesPerSecond >= SLOW_RATE) {
      return null
    }
    // 读取只占了这段时间的一小部分：慢在计算，不在盘
    if (busyMs < spanMs * BUSY_SHARE) {
      return null
    }
    if (reads >= MIN_READS && recent.averageReadBytes < SMALL_READ) {
      return { kind: 'scattered', ...base }
    }
    return { kind: 'slowDevice', ...base }
  })

  return { health }
}
