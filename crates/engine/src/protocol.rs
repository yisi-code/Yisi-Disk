//! 宿主与引擎之间的方法、通知与参数。消息本身是 JSON-RPC 2.0（见 `yisi-rpc`），经引擎的 stdin / stdout 传递。

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
/// 宿主核对改写分区表的计划时用（与分区表的读写同一份）。
pub use yisi_part::boot_sector::{MBR_TABLE_OFFSET, SIGNATURE_OFFSET, has_signature};

/// 输出目录里写入中途的文件名后缀（写完才改成正式名字）：宿主不打开以它结尾的文件。
pub use yisi_carve::PARTIAL_SUFFIX;
/// 写入计划（只是数据）：引擎生成，宿主或磁盘代理执行。
pub use yisi_io::{DeviceIdentity, SectorWrite, WritePlan, image_identity};

/// 返回 [`Capabilities`]。
pub const CAPABILITIES: &str = "engine.capabilities";
/// 读取扫描（任务）：参数 [`ReadScanParams`]，立即返回空对象，结果经 [`COMPLETED`] 通知送达，内容是 [`ReadScanReport`]。
pub const READ_SCAN: &str = "disk.readScan";
/// 取消任务：参数 [`JobRef`]。
pub const CANCEL: &str = "job.cancel";
/// 暂停任务：参数 [`JobRef`]，立即返回空对象，任务在下一个检查点停下，不再读盘。
/// 目前只有深度恢复（[`CARVE_START`]）、丢失分区搜索（[`SEARCH_PARTITIONS`]）与文件系统检查（[`FS_CHECK`]）可以暂停，
/// 其他任务答复 `INVALID_PARAMS`。
/// 暂停的时长不计入报告的用时。
pub const PAUSE: &str = "job.pause";
/// 继续暂停的任务：参数 [`JobRef`]，立即返回空对象。没暂停时什么也不做。
pub const RESUME: &str = "job.resume";
/// 列出物理磁盘：没有参数，结果是 [`DiskList`]，每块盘带上它的盘符与卷的容量。只向系统查询设备与卷的属性、
/// 不直接读磁盘，不需要管理员权限；目前只在 Windows 上提供。
pub const LIST_DISKS: &str = "disk.list";
/// 列出深度恢复支持的格式：没有参数，结果是 [`FileTypeList`]。
pub const LIST_FILE_TYPES: &str = "carve.listFileTypes";
/// 深度恢复（任务）：参数 [`CarveParams`]，立即返回 [`CarveStarted`]；找到的文件经 [`FOUND`] 通知分批送达，结果是 [`CarveSummary`]。
/// 给了输出目录时边扫边保存，源为物理磁盘时输出目录必须在另一块盘上，否则答复 [`error_code::OUTPUT_ON_SOURCE`]；
/// 不给时只扫描、什么都不写，之后用 [`CARVE_EXTRACT`] 保存选中的文件。
pub const CARVE_START: &str = "carve.start";
/// 保存「只扫描」时找到的文件（任务）：参数 [`ExtractParams`]，立即返回 [`CopyStarted`]；
/// 每个文件在源上那一段重新跑一遍深度恢复，起点与长度和扫描时一致才保存。保存结果经 [`COPIED`] 分批通知
/// （路径即文件名），结果是 [`CopySummary`]。输出目录的规则同 [`CARVE_START`]。
pub const CARVE_EXTRACT: &str = "carve.extract";
/// 预览源上的一段：参数 [`PreviewRangeParams`]，结果是 [`FilePreview`]，规则同 [`PREVIEW_FILE`]。
/// 用来预览只扫描、还没保存的文件。
pub const PREVIEW_RANGE: &str = "preview.range";
/// 预览一个文件：参数 [`PreviewParams`]，结果是 [`FilePreview`]。只读打开；JPEG、PNG 在引擎里解码、缩小后重新编码成 PNG，
/// 界面只显示这份缩略图，不直接渲染恢复出的文件。打不开时答复 [`error_code::PREVIEW_UNAVAILABLE`]。
pub const PREVIEW_FILE: &str = "preview.file";
/// 浏览分区（任务）：参数 [`BrowseParams`]，读出分区表后答复 [`BrowseOpened`]，之后一直运行到取消或宿主断开，
/// 期间接受 [`LIST_FILES`]、[`PREVIEW_FS_FILE`]、[`SCAN_DELETED`] 与 [`CANCEL_SCAN`]。只读源盘。
pub const BROWSE_OPEN: &str = "repair.open";
/// 列出浏览中的一个目录的一页：参数 [`ListFilesParams`]，结果是 [`FileListPage`]。目录第一次被列出时整份读出、排好序缓存。
pub const LIST_FILES: &str = "repair.listFiles";
/// 预览浏览中的一个文件：参数 [`PreviewFsParams`]，结果是 [`FilePreview`]，规则与 [`PREVIEW_FILE`] 相同。
pub const PREVIEW_FS_FILE: &str = "repair.previewFile";
/// 找出浏览中的一个分区里已删除的项：参数 [`ScanParams`]，扫完答复 [`ScanResult`]。NTFS 扫一遍 MFT
/// （[`PartitionInfo::needs_scan`]，不扫列不出已删除的项）；FAT / exFAT 按需读一遍目录树没用到的空闲簇，找出原来的
/// 目录已找不到的文件夹（[`PartitionInfo::optional_scan`]），列在根目录下的「找不到原目录的文件」里。
/// NTFS 扫完 MFT 后再读 `$LogFile`，找回 MFT 记录已被重用或清掉、日志里还留着的已删除项（[`FsEntry::from_log`]）。
/// 扫描期间浏览任务发 [`PROGRESS`]（`stage` 为 `scanMft`、`scanLog` 或 `scanFree`，总字节还不知道时 `total` 为 0）。
/// 同一个分区扫过之后直接答复上次的结果。
pub const SCAN_DELETED: &str = "repair.scanDeleted";
/// 停止正在进行的 [`SCAN_DELETED`]：参数 [`ScanParams`]，立即返回空对象；那次扫描答复 `cancelled: true`。
pub const CANCEL_SCAN: &str = "repair.cancelScan";
/// 把分区里选中的文件与目录复制到另一块盘（任务）：参数 [`CopyParams`]，立即返回 [`CopyStarted`]；
/// 复制好的文件经 [`COPIED`] 通知分批送达，结果是 [`CopySummary`]。源为物理磁盘时，输出目录必须在另一块盘上，
/// 否则答复 [`error_code::OUTPUT_ON_SOURCE`]。
pub const COPY_FILES: &str = "repair.copyFiles";
/// 搜索丢失的分区（任务，只读，可以暂停）：参数 [`SearchParams`]，立即返回空对象；按 [`SearchMode`] 读源，
/// 进度经 [`PROGRESS`] 送达，找到的分区有变化时经 [`PARTITIONS`] 送达当前的全部候选分区，结果是 [`SearchSummary`]。
/// 被取消时不答复 [`CANCELLED`]，而是照样以 [`COMPLETED`] 给出停下之前找到的分区（[`SearchSummary::stopped_at`]）。
pub const SEARCH_PARTITIONS: &str = "repair.search";
/// 在浏览会话里按位置打开一个分区（不在分区表里，例如搜索找到的）：参数 [`OpenRangeParams`]，
/// 结果是 [`PartitionInfo`]，编号从 [`RANGE_PARTITION_BASE`] 起，之后像表里的分区一样列目录、预览、扫描、复制。
/// 同一位置再打开时答复同一个编号。范围超出设备时答复 `INVALID_PARAMS`。
pub const OPEN_RANGE: &str = "repair.openRange";
/// 诊断引导扇区：参数 [`DiagnoseBootParams`]，结果是 [`BootDiagnosis`]。读分区表，对每个分区读出主引导扇区与备份引导扇区，
/// 逐项核对、比较主备，核对分区表里的类型与长度，给出结论。只读，读完即答复（不是任务）；分区表读不出时答复
/// [`error_code::SOURCE_UNAVAILABLE`]。
pub const DIAGNOSE_BOOT: &str = "repair.diagnoseBoot";
/// 生成修复引导扇区的写入计划：参数 [`PlanBootSectorParams`]，结果是 [`BootSectorPlan`]。只读源、不写盘，读完即答复（不是任务）。
/// 生成前重新诊断，结论必须仍是这种修复要求的（主坏备好、主好备坏），否则答复 [`error_code::PLAN_NOT_APPLICABLE`]；
/// 把写入叠加在源上预演、重新诊断不是「正常」，或者要写的位置读不出等，答复 [`error_code::PLAN_UNAVAILABLE`]；
/// 生成计划时读源出过错（盘可能有坏道），答复 [`error_code::PLAN_READ_ERRORS`]。
/// 源只能是镜像文件或整块物理盘：卷上的偏移不是盘上的偏移，答复 `INVALID_PARAMS`。
pub const PLAN_BOOT_SECTOR: &str = "repair.planBootSector";
/// 生成改写分区表的写入计划：参数 [`PlanPartitionTableParams`]，结果是 [`PartitionTablePlan`]。只读源、不写盘，读完即答复。
/// 两种改法：把搜索找到的丢失分区加进分区表（那个位置要认得出 FAT、exFAT 或 NTFS，MBR 放进空的主分区表项，
/// GPT 主备两份一起改），或把类型与文件系统不符的分区改成对的类型（诊断要报类型不符）。预演写入后重新读分区表核对。
/// 错误码同 [`PLAN_BOOT_SECTOR`]。
pub const PLAN_PARTITION_TABLE: &str = "repair.planPartitionWrite";
/// 生成 NTFS `$MFT` 与 `$MFTMirr` 互相修复的写入计划：参数 [`PlanMftParams`]，结果是 [`MftPlan`]。只读源、不写盘，
/// 读完即答复。只看前 4 条记录，逐条判断哪一份可信；分区先按文件系统检查的规则核对（对不上答复 `FS_UNAVAILABLE`）。
/// 没有可以这样修的记录、或主引导扇区不能用时答复 [`error_code::PLAN_NOT_APPLICABLE`]；其余错误码同 [`PLAN_BOOT_SECTOR`]。
pub const PLAN_MFT: &str = "repair.planMftRepair";
/// 生成文件系统修复（FAT12/16/32、exFAT）的写入计划：参数 [`PlanFsFixParams`]，结果是 [`FsFixPlan`]。只读源，
/// 读完即答复（检查一遍分区，大分区要等一会儿）。没有可以这样修的问题、卷不是 FAT / exFAT、按备份检查的、
/// 两份 FAT 本来就不一致又要改 FAT（或统一两份 FAT 时还勾了别的）、还有严重问题时清除卸下标记，答复
/// [`error_code::PLAN_NOT_APPLICABLE`]；要改的太多、空闲簇不够、预演对不上答复 [`error_code::PLAN_UNAVAILABLE`]。
pub const PLAN_FS_FIX: &str = "repair.planFsFix";
/// 两份 FAT 不一致时，把以第一份为准、以第二份为准各预演一遍（只读，读完即答复）：参数 [`RateFatCopiesParams`]，
/// 结果是 `Vec<`[`FatRating`]`>`，各给出修完还剩多少问题。卷不是 FAT、按备份检查的答复
/// [`error_code::PLAN_NOT_APPLICABLE`]。
pub const RATE_FAT_COPIES: &str = "repair.rateFatCopies";
/// 把分区里的空闲簇读一遍，找出读不出的（任务，只读）：参数 [`ScanFreeParams`]，立即答复空对象；进度按字节经
/// [`PROGRESS`] 送达，结果是 [`FreeScanSummary`]。之后可以把读不出的簇交给 [`PLAN_FS_FIX`] 的 `badClusters`
/// 标成坏簇。卷不是 FAT / exFAT、按备份检查的、没有跟踪认领的答复 [`error_code::PLAN_NOT_APPLICABLE`]。
pub const SCAN_FREE: &str = "repair.scanFree";
/// 把分区里的丢失簇另存到别的盘（任务，只读源）：参数 [`SaveLostParams`]，立即返回 [`CopyStarted`]（实际目录）；
/// 每段连续的丢失簇存成一个 `lost-<起始簇>.bin`，进度按字节经 [`PROGRESS`] 送达，结果是 [`CopySummary`]。
/// 源为物理磁盘时输出目录必须在另一块盘上（[`error_code::OUTPUT_ON_SOURCE`]）。
pub const SAVE_LOST: &str = "repair.saveLost";
/// 制作能识别坏区的镜像（任务）：参数 [`ImageParams`]，建好镜像文件与坏区图后答复 [`ImageStarted`]；
/// 进展经 [`IMAGING`] 送达，结果是 [`ImageSummary`]。镜像文件或坏区图已经存在时答复 [`error_code::IMAGE_EXISTS`]；
/// 源为物理磁盘时，镜像必须在另一块盘上，否则答复 [`error_code::OUTPUT_ON_SOURCE`]。取消时保存坏区图，之后可以续做。
pub const IMAGE_CREATE: &str = "image.create";
/// 按坏区图续做镜像（任务）：参数、答复、通知与结果同 [`IMAGE_CREATE`]。坏区图不存在、读不懂，
/// 或者记录的容量、扇区大小、来源与当前的源不符时答复 [`error_code::IMAGE_MISMATCH`]。
pub const IMAGE_RESUME: &str = "image.resume";
/// 做镜像之前看一眼：参数 [`ImageInspectParams`]，结果是 [`ImageInspection`]。读源的分区表，查输出位置所在的卷；
/// 镜像已经存在时读它的坏区图，与源上现在的分区表对比，并抽查已读出的部分与源是否一致。只读，读完即答复（不是任务）。
pub const IMAGE_INSPECT: &str = "image.inspect";
/// 查看以前做过的镜像现在怎样：参数 [`ImageDescribeParams`]，结果是 [`ImageDescriptions`]。只读镜像旁边的坏区图，
/// 不碰源，不需要管理员权限。
pub const IMAGE_DESCRIBE: &str = "image.describe";
/// 文件系统一致性检查（任务，只读）：参数 [`FsCheckParams`]，立即返回空对象；进度经 [`PROGRESS`] 按阶段送达
/// （`stage` 见 [`FsCheckReport`]），结果是 [`FsCheckReport`]。可以暂停、取消。分区要在源的分区表里、偏移与长度都对得上，
/// 否则答复 [`error_code::FS_UNAVAILABLE`]。只检查、不修复。
pub const FS_CHECK: &str = "fs.check";

/// 引擎每隔 [`HEARTBEAT_INTERVAL`] 发一次，没有参数。
pub const HEARTBEAT: &str = "engine.heartbeat";
/// 参数 [`Progress`]，每秒最多 10 次（各阶段的最后一次总会发出）。
pub const PROGRESS: &str = "job.progress";
/// 参数 [`JobFound`]：一批新找到的文件，攒满 100 个或距上一批 250 毫秒时发出。
pub const FOUND: &str = "job.found";
/// 参数 [`JobCheckpoint`]：深度恢复每秒最多一次；被取消时在最后一批 [`FOUND`] 之后再发一次。宿主据此续扫。
pub const CHECKPOINT: &str = "job.checkpoint";
/// 参数 [`JobCopied`]：一批复制好（或复制失败）的文件，攒满 100 个或距上一批 250 毫秒时发出。
pub const COPIED: &str = "job.copied";
/// 参数 [`JobImaging`]：制作镜像的进展，每秒最多 5 次，结束前总会发出最后一次。
pub const IMAGING: &str = "job.imaging";
/// 参数 [`JobPartitions`]：丢失分区搜索到目前为止找到的全部分区，有变化时发出，每秒最多 2 次；
/// 最后一份在结果里（[`SearchSummary::candidates`]），不保证另发。
pub const PARTITIONS: &str = "job.partitions";
/// 参数 [`JobReading`]：任务读源的累计统计，任务运行期间每秒一次（没有变化时不发）。界面据此判断读得慢的原因。
pub const READING: &str = "job.reading";
/// 参数 [`JobLog`]。
pub const LOG: &str = "job.log";
/// 参数 [`JobCompleted`]。
pub const COMPLETED: &str = "job.completed";
/// 参数 [`JobFailed`]。
pub const FAILED: &str = "job.failed";
/// 参数 [`JobRef`]。
pub const CANCELLED: &str = "job.cancelled";

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// 引擎自定义的错误码（JSON-RPC 留给实现的 -32000 到 -32099）。
pub mod error_code {
    /// 每个引擎进程只运行一个任务。
    pub const JOB_ALREADY_RUNNING: i64 = -32001;
    /// 打不开要扫描的设备或镜像。
    pub const SOURCE_UNAVAILABLE: i64 = -32002;
    /// 没有这个任务。
    pub const JOB_NOT_FOUND: i64 = -32003;
    /// 输出目录无法创建或写入。
    pub const OUTPUT_UNAVAILABLE: i64 = -32004;
    /// 输出目录在被扫描的物理磁盘上，或者无法确认它不在：恢复出的文件会覆盖还没找回的数据。
    pub const OUTPUT_ON_SOURCE: i64 = -32005;
    /// 要预览的文件打不开或读不了。
    pub const PREVIEW_UNAVAILABLE: i64 = -32006;
    /// 分区不存在、文件系统暂不支持浏览，或文件定位信息不合法。
    pub const FS_UNAVAILABLE: i64 = -32007;
    /// 要新建的镜像文件或坏区图已经存在：可以续做，或者换一个位置。
    pub const IMAGE_EXISTS: i64 = -32008;
    /// 要续做的镜像没有坏区图、坏区图读不懂，或者不是这个源做的。
    pub const IMAGE_MISMATCH: i64 = -32009;
    /// 诊断结论已经不适用要生成的修复（例如诊断之后源被改过）：重新诊断后再来。
    pub const PLAN_NOT_APPLICABLE: i64 = -32010;
    /// 生不成写入计划：要写的位置读不出、内容已经相同、预演写入后仍修不好等。
    pub const PLAN_UNAVAILABLE: i64 = -32011;
    /// 生成写入计划时读源出过错：盘可能有坏道，先做镜像、在镜像上修，不往这块盘上写。
    pub const PLAN_READ_ERRORS: i64 = -32012;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub version: String,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScanParams {
    /// 由宿主分配，之后的通知都带着它。
    pub job_id: String,
    pub source: Source,
}

/// 要读取的对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Source {
    /// 镜像文件：raw，或 E01（按开头的签名认，后续段在同目录找）。`sector_size` 是 raw 镜像按多大的扇区读；
    /// E01 自己记着扇区大小，不用这一项。E01 只读，不能生成写入计划。
    #[serde(rename_all = "camelCase")]
    Image { path: String, sector_size: u32 },
    /// 物理磁盘。Windows 上 `handle` 是磁盘代理复制进引擎进程的只读句柄。
    #[serde(rename_all = "camelCase")]
    Disk { handle: u64 },
    /// 物理磁盘上的一个卷（盘符），整个卷当作一个没有分区表的源。`handle` 同上，`letter` 是盘符（如 `C`）：
    /// 输出只要不在这个卷上就行（同一块盘上的别的卷可以），镜像的来源标识按卷的 GUID 名。
    #[serde(rename_all = "camelCase")]
    Volume { handle: u64, letter: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskList {
    pub disks: Vec<DiskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSummary {
    pub number: u32,
    pub path: String,
    pub size: u64,
    /// 逻辑扇区大小。
    pub sector_size: u32,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    /// ata、sata、scsi、sas、nvme、usb、sd、mmc、virtual、other 之一。
    pub bus: String,
    pub removable: bool,
    /// 认盘用的标识（厂商、型号、序列号），与制作镜像时坏区图里记的来源相同：重新插拔、盘号变了也不变。
    pub identity: String,
    /// 固态盘；设备不回答这项查询时（常见于 USB 转接）为空。
    pub solid_state: Option<bool>,
    /// 系统认出的分区表格式：mbr、gpt、raw（没有分区表或认不出）；查不到时为空。
    pub partition_style: Option<String>,
    /// 这块盘上挂载在盘符上的卷，按盘符排序；没有盘符的分区不列。
    pub volumes: Vec<DiskVolume>,
}

/// 挂载在盘符上的卷（向系统查询，不读盘）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskVolume {
    /// 盘符，如 `C`。
    pub letter: String,
    /// 认卷用的标识（卷的 GUID 名），与以这个卷为源做镜像时坏区图里记的来源相同：换了盘符、重新插拔也不变。
    pub identity: String,
    pub label: String,
    pub file_system: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job_id: String,
    pub done: u64,
    pub total: u64,
    /// 任务里的哪个阶段；只有一个阶段的任务不给。浏览与复制是 `scanMft`（扫描 MFT，单位是字节）、
    /// `scanLog`（NTFS 扫完 MFT 后读 `$LogFile`，单位是字节，进度从 0 重新算）或
    /// `scanFree`（FAT / exFAT 读空闲簇，单位是字节），
    /// 制作镜像是轮次，文件系统检查见 [`FsCheckReport`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobLog {
    pub job_id: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCompleted {
    pub job_id: String,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFailed {
    pub job_id: String,
    pub message: String,
}

/// 读取扫描的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScanReport {
    pub size: u64,
    pub bytes_read: u64,
    /// 读不出的区段，相邻的已合并；最多记录 [`MAX_UNREADABLE_RANGES`] 段。
    pub unreadable: Vec<ByteRange>,
    pub unreadable_bytes: u64,
    /// 读不出的区段超过上限、没有全部列出。
    pub unreadable_ranges_truncated: bool,
    pub elapsed_ms: u64,
    /// 扇区图：把设备按 `map_cell_bytes` 一格分开（最多 [`READ_MAP_CELLS`] 格），每格一个状态（[`ReadMapCell`]）。
    #[serde(default)]
    pub map: Vec<ReadMapCell>,
    #[serde(default)]
    pub map_cell_bytes: u64,
    /// 细读过的 1 MiB 块数；细读到了上限（之后读不出的只按整块或 64 KiB 记）。
    #[serde(default)]
    pub fine_read_blocks: u64,
    #[serde(default)]
    pub fine_read_truncated: bool,
}

pub const MAX_UNREADABLE_RANGES: usize = 1000;
/// 扇区图最多这么多格；设备小于这么多 MiB 时一格 1 MiB。
pub const READ_MAP_CELLS: usize = 1024;

/// 扇区图一格的状态。坏道检测只用前四种；制作镜像不记读得慢，另有没读与没读完。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReadMapCell {
    /// 都读出了。
    Good,
    /// 都读出了，但有一次读超过 2 秒（盘在内部反复重试）。
    Slow,
    /// 有读不出的扇区。
    Partly,
    /// 整格都读不出。
    Unreadable,
    /// 整格还没读过。
    Untried,
    /// 读了一部分，其余还没读、待修边或待细读；没有读不出的。
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTypeList {
    pub types: Vec<FileType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileType {
    /// 扩展名，同时作为格式的标识。
    pub extension: String,
    /// 按内容可能改用的扩展名（例如 ZIP 里装的是 Word 文档时存成 docx）；找到的文件
    /// （[`FoundFile::extension`]）可能是其中之一，另存时照样按它找回格式。
    pub variants: Vec<String>,
    /// 英文简述；界面按扩展名取本地化文案，取不到时显示它。
    pub description: String,
    pub max_size: u64,
    pub enabled_by_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveParams {
    pub job_id: String,
    pub source: Source,
    /// 在这个目录下新建 `recovered-<任务 ID 前 8 位>` 子目录，边扫边保存；不给时只扫描、什么都不写。
    #[serde(default)]
    pub output_dir: Option<String>,
    /// 要恢复的格式（扩展名）；不给时用各格式的默认设置。
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    /// 续扫：从这里接着扫（上次最后一个 [`CHECKPOINT`]），不给时从头扫。
    #[serde(default)]
    pub start_at: u64,
    /// 续扫边扫边保存的：直接存进这个目录（上次的 `recovered-…`），不另建子目录；给出时不看 `output_dir`。
    /// 上次已经存好的文件（续扫又找到时）保留原来那份。
    #[serde(default)]
    pub continue_dir: Option<String>,
    /// 部分恢复：没能完整的文件，格式插件说得出可靠的一段时也存下（只扫描时也报出），标成不完整
    /// （[`FoundFile::incomplete`]，名字在扩展名前加 `.incomplete`）。不给时为 false，与以前一样只要完整的文件。
    #[serde(default)]
    pub keep_incomplete: bool,
    /// 两段重组：分成两段存放的照片（基线 JPEG）也找回来，拼成完整的文件（[`FoundFile::fragments`]）。
    /// 要逐位核对每张这样的照片，扫描明显变慢；不给时为 false。
    #[serde(default)]
    pub join_fragments: bool,
}

/// 深度恢复的检查点（[`CHECKPOINT`]）：到此为止找到的文件都已经通知过，从 `resume_at` 接着扫不漏不重。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCheckpoint {
    pub job_id: String,
    pub resume_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveStarted {
    /// 实际存放结果的目录（[`CarveParams::output_dir`] 下新建的子目录）；只扫描时为空。
    pub output_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFound {
    pub job_id: String,
    pub files: Vec<FoundFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundFile {
    /// 输出目录里的文件名。
    pub name: String,
    pub extension: String,
    /// 在源上的起始偏移。
    pub offset: u64,
    pub length: u64,
    /// 部分恢复存下的不完整文件：在哪里断了。完整的文件没有这一项（旧的扫描记录里也没有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<IncompleteReason>,
    /// 由两段拼成的文件（两段重组）：各段在源上的位置，按文件里的先后，第一段从 `offset` 起。
    /// 连续存放的文件没有这一项（旧的扫描记录里也没有）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<FileFragment>,
}

/// 由几段拼成的文件里的一段：在源上的起点与长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFragment {
    pub offset: u64,
    pub length: u64,
}

impl From<yisi_carve::Fragment> for FileFragment {
    fn from(fragment: yisi_carve::Fragment) -> Self {
        Self {
            offset: fragment.offset,
            length: fragment.length,
        }
    }
}

impl From<FileFragment> for yisi_carve::Fragment {
    fn from(fragment: FileFragment) -> Self {
        Self {
            offset: fragment.offset,
            length: fragment.length,
        }
    }
}

/// 不完整的文件（部分恢复）在哪里断了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IncompleteReason {
    /// 读到源的末尾还没结束，或文件头说的结尾超出了源的末尾。
    SourceEnd,
    /// 遇到读不出的块。
    Unreadable,
    /// 文件的范围里出现了别的文件头，后面多半已经被别的文件覆盖。
    Overwritten,
    /// 结构走到一半对不上了，后面多半已是别的数据（被覆盖，或文件不是连续存放的）。
    Broken,
}

impl From<yisi_carve::Incomplete> for IncompleteReason {
    fn from(reason: yisi_carve::Incomplete) -> Self {
        match reason {
            yisi_carve::Incomplete::SourceEnd => Self::SourceEnd,
            yisi_carve::Incomplete::Unreadable => Self::Unreadable,
            yisi_carve::Incomplete::Overwritten => Self::Overwritten,
            yisi_carve::Incomplete::Broken => Self::Broken,
        }
    }
}

/// 深度恢复的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveSummary {
    /// 实际存放结果的目录；只扫描时为空。
    pub output_dir: Option<String>,
    /// 找到的文件数与字节数，含不完整的。
    pub files: u64,
    pub bytes: u64,
    /// 其中不完整的文件数（部分恢复）。
    #[serde(default)]
    pub incomplete: u64,
    /// 按扩展名统计。
    pub by_type: BTreeMap<String, TypeTotal>,
    pub unreadable_bytes: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeTotal {
    pub count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractParams {
    pub job_id: String,
    /// 扫描时的源。
    pub source: Source,
    pub items: Vec<ExtractItem>,
    /// 在这个目录下新建 `recovered-<任务 ID 前 8 位>` 子目录存放结果。
    pub output_dir: String,
}

/// 只扫描时找到的一个文件（[`FoundFile`] 的位置、长度与格式）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractItem {
    pub offset: u64,
    pub length: u64,
    pub extension: String,
    /// 保存成的文件名（另存为时用户起的）；不给时用恢复时起的名字。引擎会替换掉系统不允许的字符。
    #[serde(default)]
    pub name: Option<String>,
    /// 扫描时是不完整的文件（[`FoundFile::incomplete`] 原样带回）：保存时按部分恢复重新核对，
    /// 起点与长度和扫描时一致才保存。
    #[serde(default)]
    pub incomplete: Option<IncompleteReason>,
    /// 扫描时由两段拼成的文件（[`FoundFile::fragments`] 原样带回）：保存时把这几段接起来重新核对。
    #[serde(default)]
    pub fragments: Vec<FileFragment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRangeParams {
    pub source: Source,
    pub offset: u64,
    pub length: u64,
    /// 由两段拼成的文件（[`FoundFile::fragments`]）：按这几段接起来预览，不看 `offset` 与 `length`。
    #[serde(default)]
    pub fragments: Vec<FileFragment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewParams {
    pub path: String,
}

/// 文件开头最多给出这么多字节，供十六进制视图使用。
pub const PREVIEW_HEAD_BYTES: usize = 4096;
/// 缩略图最长边的像素数。
pub const THUMBNAIL_EDGE: u32 = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilePreview {
    pub length: u64,
    /// 文件开头最多 [`PREVIEW_HEAD_BYTES`] 字节，Base64 编码。
    pub head: String,
    /// 内容是 JPEG 或 PNG 且解码成功时的缩略图。
    pub image: Option<ImagePreview>,
    /// 内容是 JPEG 或 PNG 但解码失败时的原因。
    pub image_error: Option<String>,
    /// NTFS 文件的备用数据流（有名字的 `$DATA`，最多 [`MAX_STREAM_PREVIEWS`] 个）；其他情况为空。
    #[serde(default)]
    pub streams: Vec<StreamPreview>,
}

/// 备用数据流每个给出开头这么多字节、最多列这么多个。
pub const STREAM_HEAD_BYTES: usize = 1024;
pub const MAX_STREAM_PREVIEWS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamPreview {
    pub name: String,
    pub length: u64,
    /// 开头最多 [`STREAM_HEAD_BYTES`] 字节，Base64 编码；读不出（加密、解不开的压缩）时为空。
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePreview {
    /// 原图的尺寸。
    pub width: u32,
    pub height: u32,
    /// 最长边不超过 [`THUMBNAIL_EDGE`] 的缩略图，PNG 格式，Base64 编码。
    pub thumbnail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseParams {
    pub job_id: String,
    pub source: Source,
    /// 按哪一份引导扇区打开各分区；不给时用主引导扇区。
    #[serde(default)]
    pub boot: BootCopy,
}

/// 打开卷时用哪一份引导扇区：主引导扇区坏了、备份完好时，用备份浏览和复制（[`DIAGNOSE_BOOT`] 说备份可用的分区）；
/// 主备都坏了时，用按分区里的结构推算出的引导扇区（`rebuilt`，只在内存里换上，不写盘；推算不出的分区照旧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootCopy {
    #[default]
    Primary,
    Backup,
    Rebuilt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseOpened {
    pub size: u64,
    pub sector_size: u32,
    /// mbr、gpt、none 之一。
    pub scheme: String,
    /// 读分区表时发现的问题：primaryGptInvalid、gptUnreadable、sectorSizeMismatch、extendedChainBroken、
    /// partitionTruncated、partitionOutside。
    pub notes: Vec<String>,
    pub partitions: Vec<PartitionInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionInfo {
    /// MBR 主分区 1–4、逻辑分区从 5 起；GPT 是表项序号加 1；无分区表时为 1。
    pub number: u32,
    pub offset: u64,
    pub length: u64,
    /// MBR 为 `0x0C` 这样的类型字节，GPT 为类型 GUID，无分区表时为空。
    pub type_id: String,
    /// 常见类型的用途（efiSystem、basicData、fat32 等），界面据此显示本地化名称。
    pub role: Option<String>,
    /// GPT 分区名。
    pub name: String,
    pub bootable: bool,
    pub logical: bool,
    /// fat12、fat16、fat32、exfat、ntfs、bitlocker、ext、unknown 之一；读不出引导扇区时为 unreadable。
    pub filesystem: String,
    pub label: Option<String>,
    /// 能不能浏览目录（FAT、exFAT、NTFS）。
    pub browsable: bool,
    /// 列出已删除的项之前要先 [`SCAN_DELETED`]（NTFS）。
    #[serde(default)]
    pub needs_scan: bool,
    /// 可以按需 [`SCAN_DELETED`]（FAT、exFAT）：读一遍空闲簇，找出原来的目录已找不到的文件夹。
    #[serde(default)]
    pub optional_scan: bool,
    /// 卷上的时间是 UTC（NTFS）；否则是写入时的本地时间（FAT、exFAT）。
    #[serde(default)]
    pub times_utc: bool,
}

/// 一页最多这么多项；名字很长时引擎会少给一些，保证一条答复不超过消息大小上限。
pub const MAX_PAGE: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub job_id: String,
    /// 分区编号（[`PartitionInfo::number`]）。
    pub partition: u32,
    /// 目录的定位信息（[`FsEntry::file`]）；不给时为根目录。
    #[serde(default)]
    pub dir: Option<String>,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileListPage {
    /// 目录里的总项数。
    pub total: u32,
    /// 从 [`ListFilesParams::offset`] 起的若干项，按「目录在前、名字不分大小写」排序。
    pub entries: Vec<FsEntry>,
    /// 目录项太多，没有全部列出。
    pub truncated: bool,
    /// 已删除的目录的内容已被覆盖，列不出东西。
    pub overwritten: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub short_name: Option<String>,
    /// 名字有字符无法确定（FAT 删除后丢了首字母）。
    pub name_uncertain: bool,
    pub is_dir: bool,
    pub size: u64,
    /// `yyyy-MM-dd HH:mm:ss`，卷上记录的时间。
    pub modified: Option<String>,
    pub created: Option<String>,
    pub deleted: bool,
    /// live、intact（删除了，看起来完整）、overwritten（已被覆盖）、unknown（无法判断）、
    /// cleared（数据位置已不知道，读不出：ext3 / ext4 删除时清空了长度与数据位置，或 NTFS 的日志里只有名字）之一。
    pub state: String,
    /// 定位信息，原样传回给 [`LIST_FILES`]、[`PREVIEW_FS_FILE`]、[`COPY_FILES`]。
    pub file: String,
    /// 数据在源上的起始字节偏移；没有数据、或还不知道时为空。
    pub position: Option<u64>,
    /// 存放方式暂不支持读出：compressed、encrypted、systemCompressed、reparse（链接，含 ext 的符号链接）之一。
    #[serde(default)]
    pub unsupported: Option<String>,
    /// NTFS 的 MFT 记录号。
    #[serde(default)]
    pub record: Option<u64>,
    /// 不在卷上的虚拟目录：`orphans` 是「找不到原目录的文件」。
    #[serde(default)]
    pub virtual_dir: Option<String>,
    /// 带隐藏、系统属性（回收站 `$RECYCLE.BIN`、`System Volume Information` 等，资源管理器默认不显示）。
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub system: bool,
    /// NTFS 从 `$LogFile` 找回的已删除项：MFT 里这条记录已被重用或清掉，名字与数据位置来自日志。
    /// 文件内容按删除前的运行表读，那些簇可能已被别的文件用了；`state` 为 `cleared` 时日志里只有名字，读不出。
    #[serde(default)]
    pub from_log: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    pub job_id: String,
    pub partition: u32,
}

/// 扫描结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// 被 [`CANCEL_SCAN`] 或关闭浏览停下了；其余各项为 0，下次重新扫。
    pub cancelled: bool,
    /// MFT 里的记录数、在用的记录数（FAT、exFAT 为 0）。
    pub records: u64,
    pub in_use: u64,
    /// 已删除、还留着内容的文件与目录记录数（FAT、exFAT 为 0），以及它们的名字数（有硬链接的文件有几个名字；
    /// FAT、exFAT 是认出的文件夹里的文件与子文件夹项数）。
    pub deleted: u64,
    pub deleted_names: u64,
    /// 列在「找不到原目录的文件」里的项数：NTFS 是名字数，FAT、exFAT 是文件夹数。
    pub orphans: u64,
    /// 读不出的记录数；FAT、exFAT 是读不出的簇数。
    pub unreadable: u64,
    /// FAT、exFAT：读过的空闲空间字节数。
    #[serde(default)]
    pub free_bytes: u64,
    /// NTFS：`$LogFile` 里认出的日志记录数（没有日志、读不出时为 0）。
    #[serde(default)]
    pub log_records: u64,
    /// NTFS：从 `$LogFile` 找回的名字数（MFT 里这条记录已被重用或清掉的已删除项）。
    #[serde(default)]
    pub log_names: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFsParams {
    pub job_id: String,
    pub partition: u32,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyParams {
    pub job_id: String,
    pub source: Source,
    /// 浏览时得到的分区编号、偏移与长度；引擎重新读分区表核对，对不上就拒绝。
    pub partition: PartitionRef,
    pub items: Vec<CopyItem>,
    /// 在这个目录下新建 `restored-<任务 ID 前 8 位>` 子目录存放结果。
    pub output_dir: String,
    /// 按哪一份引导扇区打开分区，与浏览时一致；不给时用主引导扇区。
    #[serde(default)]
    pub boot: BootCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionRef {
    pub number: u32,
    pub offset: u64,
    pub length: u64,
}

impl PartitionRef {
    /// 按位置打开的分区（[`OPEN_RANGE`]，编号从 [`RANGE_PARTITION_BASE`] 起）：不在分区表里。
    /// 是不是只看编号，浏览会话按编号找分区也是这个判据。
    pub fn is_outside_table(&self) -> bool {
        self.number >= RANGE_PARTITION_BASE
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyItem {
    /// 定位信息（[`FsEntry::file`]）；是目录时连同里面的全部内容一起复制。
    pub file: String,
    /// 复制到输出目录下的相对路径，一级一项（通常是它在分区里的路径）。引擎会替换掉系统不允许的字符。
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyStarted {
    /// 实际存放结果的目录。
    pub output_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCopied {
    pub job_id: String,
    pub files: Vec<CopiedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopiedFile {
    /// 输出目录下的相对路径，以 `/` 分隔（替换过非法字符、重名时加了序号）。
    pub path: String,
    pub size: u64,
    /// 复制失败的原因；成功时为空。
    pub error: Option<String>,
    /// 复制出的内容全是 0：固态硬盘开了 TRIM 时，删除文件的簇常常已被清零，这样的文件多半找不回来了。
    #[serde(default)]
    pub all_zero: bool,
    /// 保存只扫描的结果时，这个文件在源上的起始偏移（对应 [`ExtractItem::offset`]）：
    /// 保存成的名字可能与扫描时不同（另存为改了名、重名时加了序号），界面按它对上是哪个文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopySummary {
    pub output_dir: String,
    pub files: u64,
    pub bytes: u64,
    pub failed: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    pub job_id: String,
    pub source: Source,
    pub mode: SearchMode,
}

/// 怎么搜丢失的分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchMode {
    /// 只看分区通常开始的位置（1 MiB 边界、柱面边界、分区表里记的起点），找到一个分区就跳到它的末尾。
    Quick,
    /// 从头到尾读一遍。
    Deep,
}

/// 到目前为止找到的分区（[`PARTITIONS`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobPartitions {
    pub job_id: String,
    /// 按起点排序，与结果里的写法相同。
    pub candidates: Vec<FoundPartition>,
    pub candidates_truncated: bool,
}

/// 丢失分区搜索的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSummary {
    pub size: u64,
    pub sector_size: u32,
    pub mode: SearchMode,
    /// 被停下时搜到的位置（字节）；停下之前找到的分区照样列出。搜完时为空。
    pub stopped_at: Option<u64>,
    /// 现有分区表：mbr、gpt、none 之一；读不出时为 unreadable。
    pub scheme: String,
    /// 找到的分区，按起点排序。
    pub candidates: Vec<FoundPartition>,
    /// 候选分区太多，后面的没有列出。
    pub candidates_truncated: bool,
    /// 读不出、跳过的字节数；那些位置上的引导扇区找不到。
    pub unreadable_bytes: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundPartition {
    pub offset: u64,
    /// 卷的字节数，已截到设备末尾。
    pub length: u64,
    /// fat12、fat16、fat32、exfat、ntfs 之一。
    pub filesystem: String,
    pub label: Option<String>,
    /// 靠什么找到：primary（分区开头的主引导扇区）、backup（主引导扇区已坏，靠备份）、both、
    /// structure（主备引导扇区都没了，按 `$MFT` 与 `$MFTMirr` 的结构找到，只有 NTFS；长度按卷的簇数算，只能浏览、复制）。
    pub found_by: String,
    /// 浏览、复制时按哪一份引导扇区打开（只靠备份找到的为 backup，按结构找到的为 rebuilt）；原样传给 [`OPEN_RANGE`] 与 [`COPY_FILES`]。
    pub boot: BootCopy,
    /// 卷超出了设备末尾，长度已截短。
    pub truncated: bool,
    /// 与现有分区表的关系：inTable（表里有起点相同的分区）、overlaps（与表里的分区部分重叠）、lost（丢失）。
    pub relation: String,
    /// inTable、overlaps 时是表里哪个分区。
    pub table_partition: Option<u32>,
    /// 与别的候选分区重叠：其中至少一个不是真的分区。
    pub overlaps_candidate: bool,
}

/// 按位置打开的分区从这个编号起，与分区表里的编号（最多几千）分开。
pub const RANGE_PARTITION_BASE: u32 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRangeParams {
    pub job_id: String,
    pub offset: u64,
    pub length: u64,
    /// 按哪一份引导扇区打开；搜索时只靠备份找到的分区用备份。
    #[serde(default)]
    pub boot: BootCopy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageParams {
    pub job_id: String,
    pub source: Source,
    /// 镜像文件的完整路径；坏区图是同一路径加 `.map`。
    pub image_path: String,
    /// 只读这些范围（例如选中的几个分区）；不给时读整个源。只读一部分时，分区表所在的几小段
    /// （[`ImageInspection::table_areas`]）总会一起读，镜像里才认得出分区。镜像必须是稀疏文件。
    #[serde(default)]
    pub ranges: Option<Vec<ByteRange>>,
    /// 只做复制这一轮：先把完好区域读出来，读错的块与被跳过的区段留到以后。
    #[serde(default)]
    pub copy_only: bool,
    /// 开始前把这些范围的进度清掉、重新读（重做做完的分区，或者源上变过的部分）。
    #[serde(default)]
    pub reset: Vec<ByteRange>,
    /// 读取参数；不给时都用默认值。
    #[serde(default)]
    pub tuning: ImagingTuning,
}

/// 制作镜像的读取参数（界面的「高级选项」），每一项不给时用默认值。不合规定时答复 [`error_code::INVALID_PARAMS`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImagingTuning {
    /// 第 1 轮每次读多少字节：64 KiB 到 8 MiB，扇区大小的整数倍。默认 1 MiB。
    pub block_bytes: u64,
    /// 第 1 轮读错后起步跳过多少字节（连续出错时加倍）：一个扇区到 64 MiB，扇区大小的整数倍。默认 64 KiB。
    pub skip_bytes: u64,
    /// 第 4 轮把坏扇区再读几遍：0 到 [`MAX_IMAGING_RETRIES`]。默认 0（坏盘越读越坏）。
    pub retries: u32,
    /// 第 1 轮一次读超过这么多毫秒，当作走到了坏区边上：数据照收，跳过一段先读别处。0 不看。默认 2000。
    pub slow_read_ms: u64,
    /// 连续这么多毫秒没有一次读成功就停下（坏区图照常保存，可以续做）；0 不停。默认 0。
    pub stall_limit_ms: u64,
}

impl Default for ImagingTuning {
    fn default() -> Self {
        Self {
            block_bytes: 1 << 20,
            skip_bytes: 64 << 10,
            retries: 0,
            slow_read_ms: 2000,
            stall_limit_ms: 0,
        }
    }
}

/// [`ImagingTuning::block_bytes`] 的范围。
pub const MIN_IMAGING_BLOCK: u64 = 64 << 10;
pub const MAX_IMAGING_BLOCK: u64 = 8 << 20;
/// [`ImagingTuning::skip_bytes`] 的上限。
pub const MAX_IMAGING_SKIP: u64 = 64 << 20;
/// [`ImagingTuning::retries`] 的上限。
pub const MAX_IMAGING_RETRIES: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageStarted {
    pub image_path: String,
    pub map_path: String,
    /// 坏区图里记的来源标识（物理盘见 [`DiskSummary::identity`]），宿主据此记下这个镜像是哪个源做的。
    pub source_identity: String,
    pub size: u64,
}

/// 制作镜像的进展。各项字节数取自坏区图、只算本次的范围，续做时包括以前读的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobImaging {
    pub job_id: String,
    /// 当前轮次：copy（复制）、trim（修边）、scrape（细读）、retry（重试）。
    pub round: String,
    /// 本次范围的字节数；不限范围时是源的容量。
    pub size: u64,
    /// 已读出、写进镜像的字节数。
    pub read_bytes: u64,
    /// 确认读不出的字节数。
    pub bad_bytes: u64,
    /// 还没读过、待修边、待细读的字节数。
    pub untried_bytes: u64,
    pub trim_bytes: u64,
    pub scrape_bytes: u64,
    /// 最近几秒的读速；刚开始、还算不出来时为空。
    pub bytes_per_second: Option<u64>,
    /// 只做复制这一轮。
    pub copy_only: bool,
    /// 到现在为止连续读错了几次（读成功一次就回到 0）。
    #[serde(default)]
    pub errors_in_a_row: u32,
    /// 连续读错到了要提示检查连接的次数（[`ERRORS_IN_A_ROW_NOTICE`]）。
    #[serde(default)]
    pub check_connection: bool,
    /// 整个源的扇区图（按坏区图，含本次范围以外的），格子的分法与读取扫描相同。
    #[serde(default)]
    pub map: Vec<ReadMapCell>,
    #[serde(default)]
    pub map_cell_bytes: u64,
}

/// 制作镜像时连续读错这么多次，提示检查数据线与供电。
pub const ERRORS_IN_A_ROW_NOTICE: u32 = 16;

/// 制作镜像的结果。字节数只算本次的范围（`device_*` 是整个源的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSummary {
    pub image_path: String,
    pub map_path: String,
    /// 本次范围的字节数；不限范围时是源的容量。
    pub size: u64,
    pub read_bytes: u64,
    pub bad_bytes: u64,
    /// 本次范围里还没读完的字节数（未读、待修边、待细读）：设备读不到时，或者只做了复制这一轮时不为 0。
    pub unfinished_bytes: u64,
    /// 本次范围里读不出的区段，相邻的已合并；最多记录 [`MAX_UNREADABLE_RANGES`] 段。
    pub bad_ranges: Vec<ByteRange>,
    pub bad_ranges_truncated: bool,
    /// 本次只读了源的一部分。
    pub partial: bool,
    /// 只做了复制这一轮。
    pub copy_only: bool,
    /// 整个源的容量、已读出的字节数、还没读完的字节数（含本次范围以外的）。
    pub device_size: u64,
    pub device_read_bytes: u64,
    pub device_unfinished_bytes: u64,
    /// 读错之后连以前读出的位置也读不出了（多半是设备断开），在这个偏移处停下；检查连接后可以续做。
    pub source_lost_at: Option<u64>,
    /// 连续 [`ImagingTuning::stall_limit_ms`] 没有一次读成功，在这个偏移处停下；可以续做。
    #[serde(default)]
    pub stalled_at: Option<u64>,
    /// 本次运行的用时。
    pub elapsed_ms: u64,
    /// 整个源的扇区图，同 [`JobImaging::map`]。
    #[serde(default)]
    pub map: Vec<ReadMapCell>,
    #[serde(default)]
    pub map_cell_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// 任务读源的累计统计（从任务开始算）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobReading {
    pub job_id: String,
    /// 完成的读取次数（含读错的）。
    pub reads: u64,
    /// 读出的字节数。
    pub bytes: u64,
    /// 读错的次数。
    pub errors: u64,
    /// 单次用时不少于 [`SLOW_READ_MS`] 毫秒的读取次数：盘在内部反复重试，多半是坏扇区或快坏的扇区。
    pub slow_reads: u64,
    /// 花在读取上的总毫秒数。
    pub busy_ms: u64,
    /// 此刻已经连续读了多少毫秒还没读完；没在读时为 0。
    pub stalled_ms: u64,
}

/// 单次读取超过这么多毫秒算读得很吃力。
pub const SLOW_READ_MS: u64 = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInspectParams {
    pub source: Source,
    /// 镜像文件的完整路径；还不存在时只读分区表、查输出位置。
    pub image_path: String,
}

/// 做镜像之前看到的情况。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInspection {
    pub size: u64,
    pub sector_size: u32,
    /// mbr、gpt、none 之一；分区表读不出时为空。
    pub scheme: Option<String>,
    /// 源上现在的分区，与浏览时一样。
    pub partitions: Vec<PartitionInfo>,
    /// 从头到尾排好的各段：分区（`partition` 是编号）与分区以外的空隙，各带坏区图里的进展。
    pub regions: Vec<ImageRegion>,
    /// 只读一部分时总会一起读的几小段：开头与末尾各 1 MiB（分区表、GPT 备份），以及每个分区之前的 1 MiB（逻辑分区的链表），
    /// 都只取不属于任何分区的部分。
    pub table_areas: Vec<ByteRange>,
    /// 输出位置所在的卷；查不到时为空，原因在 `output_error`。
    pub output: Option<OutputInfo>,
    pub output_error: Option<String>,
    /// 这个位置已经有镜像或坏区图时的情况。
    pub existing: Option<ExistingImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRegion {
    /// 分区编号；分区以外的空隙为空。
    pub partition: Option<u32>,
    pub offset: u64,
    pub length: u64,
    pub progress: RegionProgress,
    /// 抽查结果：镜像已存在、这一段有读出的部分时才有。
    pub check: Option<RegionCheck>,
}

/// 一段里各状态的字节数（取自坏区图；还没有镜像时全部未读）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionProgress {
    pub read_bytes: u64,
    pub bad_bytes: u64,
    pub untried_bytes: u64,
    pub trim_bytes: u64,
    pub scrape_bytes: u64,
}

/// 抽查：从镜像里已读出的部分取几块，与源上同一位置比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionCheck {
    /// 比较了几块。
    pub checked: u32,
    /// 与源不一致的块数：做镜像之后源上这一段被写过。
    pub mismatched: u32,
    /// 源上读不出、没能比较的块数。
    pub unreadable: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputInfo {
    /// 文件系统名，如 NTFS、exFAT、FAT32。
    pub file_system: String,
    /// 支持稀疏文件：只有这样才能只读一部分、分几次做。
    pub sparse: bool,
    pub free_bytes: u64,
}

/// 已经存在的镜像。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingImage {
    /// ok（可以续做）、imageMissing（只有坏区图）、mapMissing（只有镜像文件）、mapUnreadable、
    /// otherSource（坏区图记的容量、扇区大小或来源与这个源不符）、lengthMismatch（镜像文件长度与容量不符）之一。
    pub state: String,
    /// 不是 ok 时的技术细节。
    pub detail: Option<String>,
    /// 坏区图里记的来源标识。
    pub source_identity: Option<String>,
    /// 镜像是稀疏文件：可以只读一部分。旧版本建的、或者建在不支持稀疏文件的卷上的不是。
    pub sparse: bool,
    /// 坏区图里记了上次开始读时的分区表（旧版本做的没有）。
    pub partitions_recorded: bool,
    /// 与上次开始读时相比，源上分区表的变化。
    pub partition_changes: Vec<PartitionChange>,
    /// 整个源的进展。
    pub progress: RegionProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionChange {
    /// added（多出来的）、removed（不见了的）、moved（起点或长度变了的）之一。
    pub kind: String,
    pub number: u32,
    /// 上次的位置（added 时为空）。
    pub before: Option<ByteRange>,
    /// 现在的位置（removed 时为空）。
    pub after: Option<ByteRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDescribeParams {
    pub image_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDescriptions {
    pub images: Vec<ImageDescription>,
}

/// 一个以前做过的镜像现在的样子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageDescription {
    pub image_path: String,
    /// ok、missing（镜像与坏区图都不在了：被删除或移走）、imageMissing、mapMissing、mapUnreadable 之一。
    pub state: String,
    pub detail: Option<String>,
    pub source_identity: Option<String>,
    pub size: Option<u64>,
    /// 整个源的进展（state 为 ok 时）。
    pub progress: Option<RegionProgress>,
    /// 坏区图最后保存的时间（毫秒，Unix 纪元起）。
    pub saved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseBootParams {
    pub source: Source,
}

/// 引导扇区诊断的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDiagnosis {
    pub size: u64,
    pub sector_size: u32,
    /// mbr、gpt、none 之一。
    pub scheme: String,
    /// 读分区表时发现的问题，同 [`BrowseOpened::notes`]。
    pub notes: Vec<String>,
    pub partitions: Vec<PartitionBoot>,
}

/// 一个分区的诊断结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionBoot {
    /// 编号、位置与类型，含义同 [`PartitionInfo`] 的同名字段。
    pub number: u32,
    pub offset: u64,
    pub length: u64,
    pub type_id: String,
    pub role: Option<String>,
    pub name: String,
    pub logical: bool,
    /// fat12、fat16、fat32、exfat、ntfs、bitlocker、ext、unknown 之一：先按主引导扇区认，再按备份，最后按分区类型推测。
    pub filesystem: String,
    /// 结论：healthy（正常）、backupDamaged（主引导扇区正常、备份有问题）、primaryDamaged（主引导扇区坏了、备份可用）、
    /// bothDamaged（主备都坏了，或没有备份）、unsupported（BitLocker、ext，不诊断）、unrecognized（认不出文件系统）之一。
    pub verdict: String,
    /// 能打开的那一份读出的卷标（先主后备）。
    pub label: Option<String>,
    pub primary: BootCopyReport,
    /// 备份引导扇区；FAT12/16 没有备份，认不出文件系统时也不找。
    pub backup: Option<BootCopyReport>,
    /// 主备之间不一样的字段（两份都认得出是这种文件系统时才比较）。
    pub differences: Vec<BootDifference>,
    /// 分区表与引导扇区不符的地方：code 为 typeMismatch、volumeLarger、volumeSmaller；
    /// 后两种 value 是引导扇区记的卷长度、expected 是分区长度（字节）。
    pub table_issues: Vec<BootIssue>,
}

/// 一份引导扇区的核对结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootCopyReport {
    /// 在源上的字节偏移。
    pub offset: u64,
    /// 让这一份不能用的问题；为空表示这一份正常。
    pub issues: Vec<BootIssue>,
    /// 不影响使用的问题，只作提醒（核对规则与打开卷、文件系统检查相同，见 `yisi_fs::bpb`）。
    #[serde(default)]
    pub notes: Vec<BootIssue>,
    pub volume_bytes: Option<u64>,
    pub cluster_size: Option<u64>,
    /// 引导扇区正常时按它打开了卷：能打开为 `true`；打不开时 `open_error` 是原因。引导扇区有问题时不试，两项都为空。
    pub opened: bool,
    pub open_error: Option<String>,
}

/// 一个问题。`code` 为 unreadable、blank、missingSignature、badJump、badOemId、badLegacyFields、badBytesPerSector、
/// sectorSizeMismatch、badSectorsPerCluster、badReservedSectors、badFatCount、badFatSize、badMedia、badRootEntries、
/// badTotalSectors、badLayout、badRootCluster、badActiveFat、badBackupSector、badRecordSize、badIndexSize、mftOutside、mirrorOutside、checksumMismatch
/// 之一（分区表的问题见 [`PartitionBoot::table_issues`]）。`value` 是引导扇区里记的值，`expected` 是应有的值（有的话）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootIssue {
    pub code: String,
    #[serde(default)]
    pub value: Option<u64>,
    #[serde(default)]
    pub expected: Option<u64>,
}

/// 主备之间不一样的一个字段：`field` 为规范里的字段名（bytesPerSector、clusterHeapOffset、volumeSerial 这样），
/// 与文件系统检查里的 `field` 同名，界面按 `bootField.<field>` 显示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDifference {
    pub field: String,
    pub primary: u64,
    pub backup: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanBootSectorParams {
    pub source: Source,
    /// 分区表里的编号（诊断结果里的 `number`）。
    pub partition: u32,
    pub fix: BootFix,
}

/// 修复引导扇区的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootFix {
    /// 主引导扇区坏了、备份可用：把备份复制到分区开头。
    PrimaryFromBackup,
    /// 主引导扇区正常、备份坏了：把主引导扇区复制到它记的备份位置。
    BackupFromPrimary,
    /// 主备都坏了（或两处都认不出）：按分区里的结构推算出引导扇区，主引导扇区与备份都写（FAT32、exFAT、NTFS；
    /// FAT12/16 没有备份，只写主引导扇区）。
    /// 卷序列号推不出来，换一个新的。
    Rebuild,
}

/// 重建出的引导扇区：推算出的各项（规范字段名，界面按 `bootField.*` 显示）、卷标，以及按它打开后根目录里
/// 没删除的项的前几个名字与总数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuiltBoot {
    pub fields: Vec<BootField>,
    pub label: Option<String>,
    pub root: Vec<String>,
    pub root_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootField {
    pub field: String,
    pub value: u64,
}

/// 修复引导扇区的写入计划。FAT32、NTFS 只复制引导扇区本身，exFAT 复制整个引导区（12 个扇区）；
/// 重建时主引导扇区与备份各写一段（FAT32 连同 FSInfo），`from`、`to` 都是分区起点，`length` 是第一段的长度。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootSectorPlan {
    pub plan: WritePlan,
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    /// 同 [`PartitionBoot::filesystem`]。
    pub filesystem: String,
    pub fix: BootFix,
    /// 从哪里复制、写到哪里（源上的字节偏移），复制多少字节。
    pub from: u64,
    pub to: u64,
    pub length: u64,
    /// 重建时推算出的内容。
    pub rebuilt: Option<RebuiltBoot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanPartitionTableParams {
    pub source: Source,
    pub fix: PartitionTableFix,
}

/// 怎么改分区表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PartitionTableFix {
    /// 把搜索找到的丢失分区（起点与长度，字节）加进分区表。
    AddLost { offset: u64, length: u64 },
    /// 把这个编号的分区的类型改成与文件系统相符的值（主分区、逻辑分区、GPT 分区都行）。
    SetType { partition: u32 },
    /// 主 GPT 或备份 GPT 坏了：用完好的那一份修好它（方向由引擎按哪份完好定）。
    RepairGpt,
}

/// 改写分区表的写入计划，以及改完之后这个分区在表里的样子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionTablePlan {
    pub plan: WritePlan,
    pub fix: PartitionTableFix,
    /// mbr、gpt 之一。
    pub scheme: String,
    /// 同 [`PartitionBoot::filesystem`]。
    pub filesystem: String,
    /// 改完之后的分区编号、起点与长度。
    pub partition: u32,
    pub offset: u64,
    pub length: u64,
    /// 改完之后的类型：MBR 为 `0x07` 这样的类型字节，GPT 为类型 GUID。
    pub type_id: String,
    /// 改完之后的用途，同 [`PartitionInfo::role`]。
    pub role: Option<String>,
    /// 修 GPT 时的方向：primaryFromBackup（按备份重写主表）、backupFromPrimary（按主表重写备份）；此时上面的
    /// 分区编号、位置为 0，类型为空。
    #[serde(default)]
    pub gpt_repair: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanMftParams {
    pub source: Source,
    /// 检查时的分区编号、偏移与长度（同 [`FsCheckParams::partition`]）。
    pub partition: PartitionRef,
    pub fix: MftFix,
}

/// `$MFT` 与 `$MFTMirr` 互相修复的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MftFix {
    /// 用 `$MFT` 重写 `$MFTMirr`（`$MFT` 里可信、又与镜像不同的记录）。
    MirrorFromMft,
    /// 用 `$MFTMirr` 修复 `$MFT`（`$MFT` 里坏了、镜像里可信的记录）。
    MftFromMirror,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanFsFixParams {
    pub source: Source,
    /// 检查时的分区编号、偏移与长度（同 [`FsCheckParams::partition`]）。
    pub partition: PartitionRef,
    /// 修哪几类问题。
    pub fixes: Vec<FsFix>,
    /// 修坏簇时：读空闲簇时读不出的簇（[`SCAN_FREE`] 的结果），引擎再读一次核对。
    #[serde(default)]
    pub bad_clusters: Vec<ClusterRun>,
}

/// 一段连续的簇。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRun {
    pub first: u32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateFatCopiesParams {
    pub source: Source,
    /// 同 [`FsCheckParams::partition`]。
    pub partition: PartitionRef,
}

/// 以某一份 FAT 为准统一之后的样子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FatRating {
    /// [`FsFix::FatFromFirst`] 或 [`FsFix::FatFromSecond`]。
    pub fix: FsFix,
    /// 修完还剩的问题。
    pub totals: FsIssueTotals,
    /// 要改的扇区数。
    pub sectors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanFreeParams {
    pub job_id: String,
    pub source: Source,
    /// 同 [`FsCheckParams::partition`]。
    pub partition: PartitionRef,
}

/// 读空闲簇的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeScanSummary {
    /// 读了的空闲簇数与字节数。
    pub clusters: u64,
    pub bytes: u64,
    /// 读不出的簇，按簇号排序；最多 [`MAX_UNREADABLE_RUNS`] 段，再多的只计入 `unreadable_clusters`。
    pub unreadable: Vec<ClusterRun>,
    pub unreadable_clusters: u64,
    pub elapsed_ms: u64,
}

/// [`FreeScanSummary::unreadable`] 最多列这么多段。
pub const MAX_UNREADABLE_RUNS: usize = 10_000;

/// 文件系统修复的一类问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FsFix {
    /// FAT32 的 FSInfo 签名不对：换成新的。
    FsInfo,
    /// exFAT 文件占用的簇在分配位图里是空闲的：标成已占用。
    Bitmap,
    /// 链比文件大小需要的长：截短，多出来、只属于这个文件的簇释放。
    TooLong,
    /// 丢失簇：释放（先另存，[`SAVE_LOST`]）。
    Lost,
    /// 未正常卸下：标回正常卸下（别的严重问题都修好时）。
    Dirty,
    /// 两份 FAT 不一致：以第一份为准、以第二份为准（要单独做，先看 [`RATE_FAT_COPIES`]）。
    FatFromFirst,
    FatFromSecond,
    /// 断了的链：截到最后一个好簇、文件大小跟着改；起始簇就不合法的改为空文件。
    Chains,
    /// 交叉链接：后认领的文件把共用的簇复制到空闲簇。
    CrossLinks,
    /// 目录项：exFAT 的次项个数、文件名长度与校验和，孤立项与对不上的长文件名项，`.` 与 `..`，有效长度。
    Entries,
    /// 读不出的空闲簇标成坏簇（[`PlanFsFixParams::bad_clusters`]）。
    BadClusters,
    /// exFAT 大写表坏了或找不到：写回标准大写表。
    Upcase,
    /// exFAT 分配位图本身坏了：按目录树重建（要单独做）。
    RebuildBitmap,
}

/// 一类修复改了多少：`items` 是文件数（链过长）或段数，`clusters` 是簇数（FSInfo 与卸下标志为 0）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsFixCount {
    pub fix: FsFix,
    pub items: u64,
    pub clusters: u64,
}

/// 文件系统修复的写入计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsFixPlan {
    pub plan: WritePlan,
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    /// 同 [`PartitionBoot::filesystem`]。
    pub filesystem: String,
    pub counts: Vec<FsFixCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveLostParams {
    pub job_id: String,
    pub source: Source,
    /// 同 [`FsCheckParams::partition`]。
    pub partition: PartitionRef,
    /// 在这个目录下新建 `lost-<任务 ID 前 8 位>` 子目录存放结果。
    pub output_dir: String,
}

/// `$MFT` 与 `$MFTMirr` 互相修复的写入计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MftPlan {
    pub plan: WritePlan,
    pub fix: MftFix,
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    /// 要改写的记录号（0 到 3）。
    pub records: Vec<u64>,
    /// 两份前 4 条记录在源上的起点：从哪一份复制到哪一份。
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCheckParams {
    pub job_id: String,
    pub source: Source,
    /// 浏览时得到的分区编号、偏移与长度；引擎重新读分区表核对，对不上就拒绝。
    pub partition: PartitionRef,
}

/// 文件系统检查的结果。进度的阶段（`stage`）依次是：`boot`（引导区）、`fat`（读一遍 FAT 或分配位图，单位是簇）、
/// `directories`（遍历目录树，单位是已认领 / 已占用的簇）、`clusters`（逐簇汇总，单位是簇）；NTFS 是 `boot`、`mft`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCheckReport {
    /// fat12、fat16、fat32、exfat、ntfs、unknown（主、备引导扇区都认不出）之一。
    pub filesystem: String,
    pub cluster_size: u64,
    pub cluster_count: u64,
    /// FAT 或分配位图里记为已占用的簇数（NTFS 为 0）。
    pub used_clusters: u64,
    /// 遍历到的文件与目录数（不含根目录）。
    pub files: u64,
    pub directories: u64,
    /// 按发现的顺序；最多 1000 项，名字很长时引擎会少给一些，保证一条消息不超过大小上限。
    pub issues: Vec<FsIssue>,
    /// 没有列出的问题数。
    pub omitted_issues: u64,
    /// 各严重程度的问题数，含没有列出的。
    pub totals: FsIssueTotals,
    /// 读不出的字节数（相同位置只算一次）。
    pub unreadable_bytes: u64,
    /// 做了的检查与没能做的检查：boot、backupBoot、bootChecksum、fatCopies、chains、entries、crossLinks、
    /// lostClusters、bitmap、upcase、mft、mftMirror。
    pub checked: Vec<String>,
    pub skipped: Vec<String>,
    pub elapsed_ms: u64,
    /// FAT 与 exFAT 上能修的几类问题与各自多少（[`PLAN_FS_FIX`]）；坏簇要先读空闲簇，不在里面。
    #[serde(default)]
    pub repairs: Vec<FsFixCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsIssueTotals {
    pub data_loss: u64,
    pub structure: u64,
    pub minor: u64,
}

/// 检查发现的一个问题。界面按 `kind` 给出说明与以后的修法；没用到的位置信息为空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsIssue {
    /// 问题的类型，如 chainLoop、crossLink、lostClusters、entrySetChecksum、bootChecksum、unreadable。
    pub kind: String,
    /// dataLoss（会丢数据）、structure（结构错误）、minor（可以忽略）之一。
    pub severity: String,
    /// 问题出在哪个结构上：boot、backupBoot、fsInfo、fat、bitmap、upcase、rootDir、directory、mft、mftMirror。
    pub area: Option<String>,
    /// 涉及的文件或目录，以 `/` 开头；`$Bitmap`、`$UpCase` 是 exFAT 的分配位图与大写表。
    pub path: Option<String>,
    /// 交叉链接的另一个文件。
    pub other_path: Option<String>,
    pub cluster: Option<u64>,
    pub count: Option<u64>,
    /// 在源上的字节偏移（读不出的区段）。
    pub offset: Option<u64>,
    pub expected: Option<u64>,
    pub actual: Option<u64>,
    /// 引导扇区里出问题的那一项，如 bytesPerSector。
    pub field: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn read_scan_params_wire_format() {
        let params = ReadScanParams {
            job_id: "j1".to_owned(),
            source: Source::Image {
                path: "a.img".to_owned(),
                sector_size: 512,
            },
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"jobId": "j1", "source": {"type": "image", "path": "a.img", "sectorSize": 512}})
        );
    }

    #[test]
    fn volume_sources_carry_the_handle_and_letter() {
        let source: Source =
            serde_json::from_value(json!({"type": "volume", "handle": 12, "letter": "D"})).unwrap();
        assert_eq!(
            source,
            Source::Volume {
                handle: 12,
                letter: "D".to_owned()
            }
        );
    }

    #[test]
    fn partitions_outside_the_table_are_known_by_number() {
        let listed: PartitionRef =
            serde_json::from_value(json!({"number": 2, "offset": 1024, "length": 4096})).unwrap();
        assert!(!listed.is_outside_table());
        let found: PartitionRef = serde_json::from_value(json!({
            "number": RANGE_PARTITION_BASE, "offset": 1024, "length": 4096
        }))
        .unwrap();
        assert!(found.is_outside_table());
        let range: OpenRangeParams =
            serde_json::from_value(json!({"jobId": "b1", "offset": 1024, "length": 4096})).unwrap();
        assert_eq!(range.boot, BootCopy::Primary);
    }

    #[test]
    fn boot_copy_defaults_to_the_primary() {
        let params: BrowseParams =
            serde_json::from_value(json!({"jobId": "b1", "source": {"type": "disk", "handle": 4}}))
                .unwrap();
        assert_eq!(params.boot, BootCopy::Primary);
        let params: BrowseParams = serde_json::from_value(
            json!({"jobId": "b1", "source": {"type": "disk", "handle": 4}, "boot": "backup"}),
        )
        .unwrap();
        assert_eq!(params.boot, BootCopy::Backup);
    }

    #[test]
    fn fs_check_wire_format() {
        let params: FsCheckParams = serde_json::from_value(json!({
            "jobId": "c1",
            "source": {"type": "disk", "handle": 8},
            "partition": {"number": 2, "offset": 1048576, "length": 4096}
        }))
        .unwrap();
        assert_eq!(params.partition.number, 2);
        let issue = FsIssue {
            kind: "crossLink".to_owned(),
            severity: "dataLoss".to_owned(),
            area: None,
            path: Some("/a".to_owned()),
            other_path: Some("/b".to_owned()),
            cluster: Some(7),
            count: Some(1),
            offset: None,
            expected: None,
            actual: None,
            field: None,
        };
        let value = serde_json::to_value(&issue).unwrap();
        assert_eq!(value["otherPath"], "/b");
        assert_eq!(value["area"], Value::Null);
    }
}
