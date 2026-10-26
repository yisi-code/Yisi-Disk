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
/// 列出物理磁盘：没有参数，结果是 [`DiskList`]。只查询设备属性、不读磁盘内容，不需要管理员权限；目前只在 Windows 上提供。
pub const LIST_DISKS: &str = "disk.list";
/// 列出深度恢复支持的格式：没有参数，结果是 [`FileTypeList`]。
pub const LIST_FILE_TYPES: &str = "carve.listFileTypes";
/// 深度恢复（任务）：参数 [`CarveParams`]，立即返回 [`CarveStarted`]；找到的文件经 [`FOUND`] 通知分批送达，结果是 [`CarveSummary`]。
/// 源为物理磁盘时，输出目录必须在另一块盘上，否则答复 [`error_code::OUTPUT_ON_SOURCE`]。
pub const CARVE_START: &str = "carve.start";
/// 预览一个文件：参数 [`PreviewParams`]，结果是 [`FilePreview`]。只读打开；JPEG、PNG 在引擎里解码、缩小后重新编码成 PNG，
/// 界面只显示这份缩略图，不直接渲染恢复出的文件。打不开时答复 [`error_code::PREVIEW_UNAVAILABLE`]。
pub const PREVIEW_FILE: &str = "preview.file";
/// 浏览分区（任务）：参数 [`BrowseParams`]，读出分区表后答复 [`BrowseOpened`]，之后一直运行到取消或宿主断开，
/// 期间接受 [`LIST_FILES`] 与 [`PREVIEW_FS_FILE`]。只读源盘。
pub const BROWSE_OPEN: &str = "repair.open";
/// 列出浏览中的一个目录的一页：参数 [`ListFilesParams`]，结果是 [`FileListPage`]。目录第一次被列出时整份读出、排好序缓存。
pub const LIST_FILES: &str = "repair.listFiles";
/// 预览浏览中的一个文件：参数 [`PreviewFsParams`]，结果是 [`FilePreview`]，规则与 [`PREVIEW_FILE`] 相同。
pub const PREVIEW_FS_FILE: &str = "repair.previewFile";
/// 把分区里选中的文件与目录复制到另一块盘（任务）：参数 [`CopyParams`]，立即返回 [`CopyStarted`]；
/// 复制好的文件经 [`COPIED`] 通知分批送达，结果是 [`CopySummary`]。源为物理磁盘时，输出目录必须在另一块盘上，
/// 否则答复 [`error_code::OUTPUT_ON_SOURCE`]。
pub const COPY_FILES: &str = "repair.copyFiles";

/// 引擎每隔 [`HEARTBEAT_INTERVAL`] 发一次，没有参数。
pub const HEARTBEAT: &str = "engine.heartbeat";
/// 参数 [`Progress`]，每秒最多 10 次。
pub const PROGRESS: &str = "job.progress";
/// 参数 [`JobFound`]：一批新找到的文件，攒满 100 个或距上一批 250 毫秒时发出。
pub const FOUND: &str = "job.found";
/// 参数 [`JobCopied`]：一批复制好（或复制失败）的文件，攒满 100 个或距上一批 250 毫秒时发出。
pub const COPIED: &str = "job.copied";
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
    /// 在这个目录下新建 `recovered-<任务 ID 前 8 位>` 子目录存放结果。
    pub output_dir: String,
    /// 要恢复的格式（扩展名）；不给时用各格式的默认设置。
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveStarted {
    /// 实际存放结果的目录（[`CarveParams::output_dir`] 下新建的子目录）。
    pub output_dir: String,
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
    /// 实际存放结果的目录。
    pub output_dir: String,
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
    /// 能不能浏览目录（目前是 FAT 与 exFAT）。
    pub browsable: bool,
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
    /// 数据在源上的起始字节偏移；没有数据时为空。
    pub position: Option<u64>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionRef {
    pub number: u32,
    pub offset: u64,
    pub length: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
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
}
