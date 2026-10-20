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
}

export default enUS
