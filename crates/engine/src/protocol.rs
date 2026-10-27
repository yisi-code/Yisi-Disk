//! 宿主与引擎之间的方法、通知与参数。消息本身是 JSON-RPC 2.0（见 `yisi-rpc`），经引擎的 stdin / stdout 传递。

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
/// 列出物理磁盘：没有参数，结果是 [`DiskList`]。只查询设备属性、不读磁盘内容，不需要管理员权限；目前只在 Windows 上提供。
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
/// 找出浏览中的一个分区里已删除的项（NTFS 要扫一遍 MFT，其他文件系统立即答复）：参数 [`ScanParams`]，
/// 扫完答复 [`ScanResult`]。扫描期间浏览任务发 [`PROGRESS`]（`stage` 为 `scanMft`）。同一个分区扫过之后直接答复上次的结果。
pub const SCAN_DELETED: &str = "repair.scanDeleted";
/// 停止正在进行的 [`SCAN_DELETED`]：参数 [`ScanParams`]，立即返回空对象；那次扫描答复 `cancelled: true`。
pub const CANCEL_SCAN: &str = "repair.cancelScan";
/// 把分区里选中的文件与目录复制到另一块盘（任务）：参数 [`CopyParams`]，立即返回 [`CopyStarted`]；
/// 复制好的文件经 [`COPIED`] 通知分批送达，结果是 [`CopySummary`]。源为物理磁盘时，输出目录必须在另一块盘上，
/// 否则答复 [`error_code::OUTPUT_ON_SOURCE`]。
pub const COPY_FILES: &str = "repair.copyFiles";
/// 搜索丢失的分区（任务，只读，可以暂停）：参数 [`SearchParams`]，立即返回空对象；从头到尾读一遍源，
/// 进度经 [`PROGRESS`] 送达，结果是 [`SearchSummary`]。
pub const SEARCH_PARTITIONS: &str = "repair.search";
/// 在浏览会话里按位置打开一个分区（不在分区表里，例如搜索找到的）：参数 [`OpenRangeParams`]，
/// 结果是 [`PartitionInfo`]，编号从 [`RANGE_PARTITION_BASE`] 起，之后像表里的分区一样列目录、预览、扫描、复制。
/// 同一位置再打开时答复同一个编号。范围超出设备时答复 `INVALID_PARAMS`。
pub const OPEN_RANGE: &str = "repair.openRange";
/// 诊断引导扇区：参数 [`DiagnoseBootParams`]，结果是 [`BootDiagnosis`]。读分区表，对每个分区读出主引导扇区与备份引导扇区，
/// 逐项核对、比较主备，核对分区表里的类型与长度，给出结论。只读，读完即答复（不是任务）；分区表读不出时答复
/// [`error_code::SOURCE_UNAVAILABLE`]。
pub const DIAGNOSE_BOOT: &str = "repair.diagnoseBoot";
/// 制作能识别坏区的镜像（任务）：参数 [`ImageParams`]，建好镜像文件与坏区图后答复 [`ImageStarted`]；
/// 进展经 [`IMAGING`] 送达，结果是 [`ImageSummary`]。镜像文件或坏区图已经存在时答复 [`error_code::IMAGE_EXISTS`]；
/// 源为物理磁盘时，镜像必须在另一块盘上，否则答复 [`error_code::OUTPUT_ON_SOURCE`]。取消时保存坏区图，之后可以续做。
pub const IMAGE_CREATE: &str = "image.create";
/// 按坏区图续做镜像（任务）：参数、答复、通知与结果同 [`IMAGE_CREATE`]。坏区图不存在、读不懂，
/// 或者记录的容量、扇区大小、来源与当前的源不符时答复 [`error_code::IMAGE_MISMATCH`]。
pub const IMAGE_RESUME: &str = "image.resume";
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
/// 参数 [`JobCopied`]：一批复制好（或复制失败）的文件，攒满 100 个或距上一批 250 毫秒时发出。
pub const COPIED: &str = "job.copied";
/// 参数 [`JobImaging`]：制作镜像的进展，每秒最多 5 次，结束前总会发出最后一次。
pub const IMAGING: &str = "job.imaging";
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
    #[serde(rename_all = "camelCase")]
    Image { path: String, sector_size: u32 },
    /// 物理磁盘。Windows 上 `handle` 是磁盘代理复制进引擎进程的只读句柄。
    #[serde(rename_all = "camelCase")]
    Disk { handle: u64 },
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
    /// 任务里的哪个阶段；只有一个阶段的任务不给。浏览与复制是 `scanMft`（扫描 MFT，单位是字节），
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
}

pub const MAX_UNREADABLE_RANGES: usize = 1000;

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
}

/// 深度恢复的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveSummary {
    /// 实际存放结果的目录；只扫描时为空。
    pub output_dir: Option<String>,
    pub files: u64,
    pub bytes: u64,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRangeParams {
    pub source: Source,
    pub offset: u64,
    pub length: u64,
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

/// 打开卷时用哪一份引导扇区：主引导扇区坏了、备份完好时，用备份浏览和复制（[`DIAGNOSE_BOOT`] 说备份可用的分区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootCopy {
    #[default]
    Primary,
    Backup,
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
    /// live、intact（删除了，看起来完整）、overwritten（已被覆盖）、unknown（无法判断）之一。
    pub state: String,
    /// 定位信息，原样传回给 [`LIST_FILES`]、[`PREVIEW_FS_FILE`]、[`COPY_FILES`]。
    pub file: String,
    /// 数据在源上的起始字节偏移；没有数据、或还不知道时为空。
    pub position: Option<u64>,
    /// 存放方式暂不支持读出：compressed、encrypted、systemCompressed、reparse 之一。
    #[serde(default)]
    pub unsupported: Option<String>,
    /// NTFS 的 MFT 记录号。
    #[serde(default)]
    pub record: Option<u64>,
    /// 不在卷上的虚拟目录：`orphans` 是「找不到原目录的文件」。
    #[serde(default)]
    pub virtual_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanParams {
    pub job_id: String,
    pub partition: u32,
}

/// 扫描结果。不需要扫描的文件系统各项为 0。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// 被 [`CANCEL_SCAN`] 或关闭浏览停下了；其余各项为 0，下次重新扫。
    pub cancelled: bool,
    /// MFT 里的记录数、在用的记录数。
    pub records: u64,
    pub in_use: u64,
    /// 已删除、还留着内容的文件与目录记录数，以及它们的名字数（有硬链接的文件有几个名字）。
    pub deleted: u64,
    pub deleted_names: u64,
    /// 原来的目录已找不到、列在「找不到原目录的文件」里的名字数。
    pub orphans: u64,
    /// 读不出的记录数。
    pub unreadable: u64,
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
    /// 按位置打开的分区（[`OPEN_RANGE`]）：不核对分区表，只核对范围在设备之内。
    #[serde(default)]
    pub outside_table: bool,
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
}

/// 丢失分区搜索的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSummary {
    pub size: u64,
    pub sector_size: u32,
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
    /// 靠哪个引导扇区找到：primary（分区开头的主引导扇区）、backup（主引导扇区已坏，靠备份）、both。
    pub found_by: String,
    /// 浏览、复制时按哪一份引导扇区打开（只靠备份找到的为 backup）；原样传给 [`OPEN_RANGE`] 与 [`COPY_FILES`]。
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageStarted {
    pub image_path: String,
    pub map_path: String,
}

/// 制作镜像的进展。各项字节数取自坏区图，续做时包括以前读的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobImaging {
    pub job_id: String,
    /// 当前轮次：copy（复制）、trim（修边）、scrape（细读）。
    pub round: String,
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
}

/// 制作镜像的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSummary {
    pub image_path: String,
    pub map_path: String,
    pub size: u64,
    pub read_bytes: u64,
    pub bad_bytes: u64,
    /// 还没读完的字节数（未读、待修边、待细读）；只在设备读不到时不为 0。
    pub unfinished_bytes: u64,
    /// 读不出的区段，相邻的已合并；最多记录 [`MAX_UNREADABLE_RANGES`] 段。
    pub bad_ranges: Vec<ByteRange>,
    pub bad_ranges_truncated: bool,
    /// 读错之后连以前读出的位置也读不出了（多半是设备断开），在这个偏移处停下；检查连接后可以续做。
    pub source_lost_at: Option<u64>,
    /// 本次运行的用时。
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
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
    /// 发现的问题；为空表示这一份正常。
    pub issues: Vec<BootIssue>,
    pub volume_bytes: Option<u64>,
    pub cluster_size: Option<u64>,
    /// 引导扇区正常时按它打开了卷：能打开为 `true`；打不开时 `open_error` 是原因。引导扇区有问题时不试，两项都为空。
    pub opened: bool,
    pub open_error: Option<String>,
}

/// 一个问题。`code` 为 unreadable、blank、missingSignature、badJump、badOemId、badLegacyFields、badBytesPerSector、
/// sectorSizeMismatch、badSectorsPerCluster、badReservedSectors、badFatCount、badFatSize、badMedia、badRootEntries、
/// badTotalSectors、badLayout、badRootCluster、badRecordSize、badIndexSize、mftOutside、mirrorOutside、checksumMismatch
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

/// 主备之间不一样的一个字段：`field` 为 bytesPerSector、sectorsPerCluster、totalSectors、volumeSerial 这样的名字。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootDifference {
    pub field: String,
    pub primary: u64,
    pub backup: u64,
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
    fn partition_ref_outside_table_fields_are_optional() {
        let listed: PartitionRef =
            serde_json::from_value(json!({"number": 2, "offset": 1024, "length": 4096})).unwrap();
        assert!(!listed.outside_table);
        let found: PartitionRef = serde_json::from_value(json!({
            "number": RANGE_PARTITION_BASE, "offset": 1024, "length": 4096, "outsideTable": true
        }))
        .unwrap();
        assert!(found.outside_table);
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
