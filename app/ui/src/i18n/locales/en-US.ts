import type zhCN from './zh-CN'

const enUS: typeof zhCN = {
  app: {
    name: 'Yisi-Disk',
    subtitle: 'Disk data recovery',
  },
  locale: {
    label: 'Language',
    'zh-CN': '中文',
    'en-US': 'English',
  },
  scenarios: {
    title: 'What do you want to do?',
    unavailable: 'Engine not connected yet',
    findPartitions: 'Find lost partitions',
    repairPartition: 'Repair a partition that will not open',
    undelete: 'Recover deleted files',
    carve: 'Deep recovery by file type',
    fsCheck: 'Check and repair a file system',
    image: 'Create a disk image (bad-sector aware)',
  },
  host: {
    connecting: 'Connecting to host…',
    failed: 'Cannot connect to host: {reason}',
    version: 'Version {version}',
  },
  selfTest: {
    title: 'Engine self-test',
    engineChecking: 'Checking the engine…',
    engineReady: 'Engine {version} ready, supports: {methods}',
    imagePath: 'Image file',
    imagePathPlaceholder: 'Full path of the image file, e.g. D:/images/disk.img',
    start: 'Start read scan',
    cancel: 'Cancel',
    cancelling: 'Cancelling…',
    unresponsive: 'The engine has not responded for over 10 seconds',
    status: {
      starting: 'Starting the engine…',
      running: 'Read {done} of {total} ({percent}%), {rate}/s',
      completed: 'Scan completed',
      failed: 'Scan failed',
      cancelled: 'Cancelled',
    },
    result: {
      bytesRead: 'Read',
      unreadable: 'Unreadable ranges',
      unreadableValue: '{count} ranges, {bytes} in total',
      elapsed: 'Elapsed',
      elapsedValue: '{seconds} s',
    },
  },
  errors: {
    withDetail: '{message} ({detail})',
    unknown: 'An unknown error occurred',
    engineNotFound: 'Engine program not found',
    engineStart: 'Cannot start the engine',
    engineIo: 'Communication with the engine failed',
    engineTimeout: 'The engine did not reply in time',
    engineRejected: 'The engine rejected the request',
    engineExited: 'The engine exited unexpectedly',
    engineProtocol: 'Unexpected reply from the engine',
    jobExists: 'The job is already running',
    jobNotFound: 'Job not found',
    invalidJobId: 'Invalid job ID',
    jobFailed: 'The job failed',
  },
}

export default enUS
