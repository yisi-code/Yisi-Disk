export default {
  app: {
    name: 'Yisi-Disk',
    subtitle: '磁盘数据恢复',
  },
  locale: {
    label: '界面语言',
    'zh-CN': '中文',
    'en-US': 'English',
  },
  scenarios: {
    title: '选择要做的事',
    unavailable: '引擎尚未接入',
    findPartitions: '找回丢失的分区',
    repairPartition: '修复打不开的分区',
    undelete: '找回删除的文件',
    carve: '按文件类型深度恢复',
    fsCheck: '检查并修复文件系统',
    image: '制作磁盘镜像（能识别坏区）',
  },
  host: {
    connecting: '正在连接宿主…',
    failed: '无法连接宿主：{reason}',
    version: '版本 {version}',
  },
}
