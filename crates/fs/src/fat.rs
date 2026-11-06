//! FAT12 / FAT16 / FAT32：引导扇区（BPB）、目录（含长文件名）、删除项识别。依据微软公开的 FAT 规范。

use yisi_part::bytes::{le16, le32};

use crate::bpb::FatBpb;
use crate::clusters::{Clusters, Link, Run};
use crate::entry::{
    ClusterRef, Entry, FileAttributes, FileRef, Listing, Location, Stamp, State, dos_time,
};
use crate::format::{
    ATTR_DIRECTORY, ATTR_LONG_NAME, ATTR_VOLUME_ID, DIR_ENTRY_SIZE as ENTRY,
    FAT_DELETED as DELETED, LfnPart, MAX_LFN_PARTS, fat_first_cluster, lfn_name, lfn_ordered,
    lfn_part, push_run,
};
use crate::orphans::{FreeSpace, Look, ScanSlot};
use crate::reader::FileReader;
use crate::region::{ChunkCache, Region};
use crate::{FsError, FsKind, MAX_DIR_BYTES, MAX_ENTRIES, ScanSummary, bpb};

/// 名字首字节确实是 0xE5 时，目录项里存的是 0x05。
const KANJI_E5: u8 = 0x05;

/// 解析引导扇区；有致命问题、或 FAT 一个簇也装不下时返回 `None`（规则在 [`bpb`](crate::bpb)，与诊断、文件系统检查共用）。
pub(crate) fn parse_boot(sector: &[u8]) -> Option<FatBpb> {
    bpb::check_fat(sector, None)
        .geometry
        .filter(|bpb| bpb.usable_clusters(u64::MAX) > 0)
}

pub(crate) struct Fat {
    kind: FsKind,
    clusters: Clusters,
    /// FAT12/16 的根目录区：分区内偏移与字节数；FAT32 为 `None`，根目录在簇里。
    fixed_root: Option<(u64, u64)>,
    root_cluster: u32,
    label: Option<String>,
    /// 读空闲簇找出的、原来的目录已找不到的文件夹。
    scan: ScanSlot,
}

impl Fat {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry =
            parse_boot(boot).ok_or(FsError::Invalid("文件分配表（FAT）引导扇区的参数不自洽"))?;
        let heap_offset = geometry.heap_offset();
        if heap_offset >= region.length() {
            return Err(FsError::Invalid("文件分配表（FAT）的数据区在分区之外"));
        }
        // 分区被截短时，簇区只算到分区末尾
        let count = u32::try_from(geometry.usable_clusters(region.length()))
            .map_err(|_| FsError::Invalid("簇数过大"))?;
        let kind = geometry.kind;
        let fixed_root = (kind != FsKind::Fat32).then(|| {
            (
                geometry.root_dir_offset(),
                geometry.root_entries * ENTRY as u64,
            )
        });
        let clusters = Clusters {
            fat: ChunkCache::new(
                region.clone(),
                geometry.active_fat_offset(),
                geometry.fat_bytes(),
            ),
            region,
            heap_offset,
            cluster_size: geometry.cluster_size(),
            count,
            width: geometry.width(),
        };
        if kind == FsKind::Fat32 && !clusters.is_valid(geometry.root_cluster) {
            return Err(FsError::Invalid(
                "32 位文件分配表（FAT32）根目录的簇号不合法",
            ));
        }
        let mut fat = Self {
            kind,
            clusters,
            fixed_root,
            root_cluster: geometry.root_cluster,
            label: geometry.label,
            scan: ScanSlot::default(),
        };
        // 根目录里的卷标比引导扇区里的准（改卷标时 Windows 只改根目录）
        if let Ok((bytes, ..)) = fat.dir_contents(&fat.root())
            && let Some(label) = root_label(&bytes)
        {
            fat.label = Some(label);
        }
        Ok(fat)
    }

    pub fn kind(&self) -> FsKind {
        self.kind
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn cluster_size(&self) -> u64 {
        self.clusters.cluster_size
    }

    pub fn root(&self) -> FileRef {
        FileRef {
            size: 0,
            is_dir: true,
            deleted: false,
            location: Location::Clusters(ClusterRef {
                first_cluster: if self.fixed_root.is_some() {
                    0
                } else {
                    self.root_cluster
                },
                valid_size: 0,
                contiguous: false,
            }),
            modified: None,
        }
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        self.list_runs(dir).map(|(listing, _)| listing)
    }

    /// 扫一遍空闲簇，找出原来的目录已找不到的文件夹（[`crate::orphans`]）。
    pub fn scan(
        &self,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        self.scan.run(self, progress)
    }

    pub fn is_scanned(&self) -> bool {
        self.scan.get().is_some()
    }

    fn is_root(&self, dir: &FileRef) -> bool {
        !dir.deleted && dir.location == self.root().location
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let at = file.clusters()?;
        let needed = self.clusters.clusters_for(file.size);
        let runs = if at.contiguous {
            self.clusters.contiguous(at.first_cluster, needed).0
        } else {
            self.clusters.chain(at.first_cluster, needed)?.runs
        };
        Ok(FileReader::new(&self.clusters, &runs, file.size, file.size))
    }

    /// 目录的原始内容与读了哪些簇；第三个值为 `true` 表示这个已删除的目录已被覆盖。
    fn dir_contents(&self, dir: &FileRef) -> Result<(Vec<u8>, Vec<Run>, bool), FsError> {
        if dir.is_fixed_root() {
            let Some((offset, length)) = self.fixed_root else {
                return Err(FsError::BadRef);
            };
            let mut bytes = vec![0u8; length as usize];
            self.clusters.region.read_exact(offset, &mut bytes)?;
            return Ok((bytes, Vec::new(), false));
        }
        let first = dir.clusters()?.first_cluster;
        let cs = self.clusters.cluster_size;
        let max_clusters = (MAX_DIR_BYTES / cs).max(1);
        if !dir.deleted {
            let chain = self.clusters.chain(first, max_clusters)?;
            let bytes = self.clusters.read_runs(&chain.runs, MAX_DIR_BYTES)?;
            return Ok((bytes, chain.runs, false));
        }
        // 扫描空闲簇认出的一组目录内容（目录在 FAT 里没有长度，只有它带长度）：就读这几个簇，
        // 不要求以 `.` 与 `..` 开头（可能只是目录中间的一段）
        if dir.size > 0 {
            let needed = self.clusters.clusters_for(dir.size.min(MAX_DIR_BYTES));
            let (runs, _) = self.clusters.contiguous(first, needed);
            let bytes = self.clusters.read_runs(&runs, MAX_DIR_BYTES)?;
            return Ok((bytes, runs, false));
        }

        // 已删除的目录：簇链已清零，只能从首簇起往后找连续的空闲簇，
        // 直到遇到目录结束标记或已被占用的簇。首簇开头必须还是 `.` 与 `..`，否则已被覆盖。
        let mut bytes = self.first_cluster(first)?;
        if !starts_with_dots(&bytes) {
            return Ok((Vec::new(), Vec::new(), true));
        }
        let mut runs = Vec::new();
        push_run(&mut runs, first);
        let mut cluster = first;
        let mut count = 1u64;
        while count < max_clusters && !has_end_marker(&bytes) {
            let Some(next) = cluster
                .checked_add(1)
                .filter(|&next| self.clusters.is_valid(next))
            else {
                break;
            };
            if self.clusters.link(next)? != Link::Free {
                break;
            }
            let (run, _) = self.clusters.contiguous(next, 1);
            bytes.extend(self.clusters.read_runs(&run, cs)?);
            push_run(&mut runs, next);
            cluster = next;
            count += 1;
        }
        Ok((bytes, runs, false))
    }

    fn entry(&self, raw: RawEntry) -> Result<Entry, FsError> {
        let deleted = raw.deleted;
        let size = if raw.is_dir { 0 } else { raw.size };
        let file = FileRef {
            size,
            is_dir: raw.is_dir,
            deleted,
            location: Location::Clusters(ClusterRef {
                first_cluster: raw.first_cluster,
                valid_size: size,
                contiguous: deleted,
            }),
            modified: raw.modified.text().is_some().then_some(raw.modified),
        };
        let state = if deleted {
            // 早年的 Windows 删除 FAT32 文件时会清掉首簇的高 16 位。高位为 0、簇数又超过 16 位时，
            // 按低位找到的簇已被占用，说不准是真被覆盖了，还是首簇本来就不在这里
            let ambiguous = self.kind == FsKind::Fat32
                && raw.cluster_high == 0
                && u64::from(self.clusters.count) + 2 > 0xFFFF;
            match self.deleted_state(&file, raw.first_cluster)? {
                State::Overwritten if ambiguous => State::Unknown,
                state => state,
            }
        } else {
            State::Live
        };
        Ok(Entry {
            name: raw.name,
            short_name: Some(raw.short_name),
            name_uncertain: raw.name_uncertain,
            size: file.size,
            modified: raw.modified.text(),
            created: raw.created.text(),
            state,
            position: (raw.is_dir || size > 0)
                .then(|| self.clusters.offset(raw.first_cluster))
                .flatten(),
            unsupported: None,
            attributes: FileAttributes::from_bits(u32::from(raw.attr)),
            file,
        })
    }

    fn deleted_state(&self, file: &FileRef, first: u32) -> Result<State, FsError> {
        if !file.is_dir && file.size == 0 {
            return Ok(State::Intact);
        }
        let needed = if file.is_dir {
            1
        } else {
            self.clusters.clusters_for(file.size)
        };
        let (runs, complete) = self.clusters.contiguous(first, needed);
        if !complete {
            return Ok(State::Unknown);
        }
        for run in &runs {
            for step in 0..run.count {
                if self.clusters.link(run.first + step)? != Link::Free {
                    return Ok(State::Overwritten);
                }
            }
        }
        // 目录的簇虽然空闲，开头却已不是 `.` 与 `..`：内容已不在（被覆盖，或删除前就没写到盘上）
        if file.is_dir && !starts_with_dots(&self.first_cluster(first)?) {
            return Ok(State::Overwritten);
        }
        Ok(State::Intact)
    }

    fn first_cluster(&self, cluster: u32) -> Result<Vec<u8>, FsError> {
        let (run, _) = self.clusters.contiguous(cluster, 1);
        self.clusters.read_runs(&run, self.clusters.cluster_size)
    }

    /// 子目录的第一个簇：以 `.`、`..` 开头（两项都带目录属性），`.` 的首簇就是这个簇自己，`..` 的首簇为 0（上级是
    /// 根目录）或合法的簇号。`.` 指向自己这一条已足够严，后面的项不再逐项核对。
    fn look_head(&self, cluster: u32, bytes: &[u8]) -> Option<Look> {
        let is_fat32 = self.kind == FsKind::Fat32;
        let (dot, dotdot) = (bytes.get(..ENTRY)?, bytes.get(ENTRY..2 * ENTRY)?);
        let directory = |raw: &[u8]| raw.get(11).is_some_and(|&attr| attr & ATTR_DIRECTORY != 0);
        let parent = fat_first_cluster(dotdot, is_fat32);
        if !starts_with_dots(bytes)
            || !directory(dot)
            || !directory(dotdot)
            || fat_first_cluster(dot, is_fat32) != cluster
            || (parent != 0 && !self.clusters.is_valid(parent))
        {
            return None;
        }
        let entries = bytes
            .as_chunks::<ENTRY>()
            .0
            .iter()
            .skip(2)
            .take_while(|raw| raw[0] != 0)
            .filter(|raw| raw[11] & 0x3F != ATTR_LONG_NAME && raw[11] & ATTR_VOLUME_ID == 0)
            .count();
        let modified = Stamp::Dos {
            stamp: (u32::from(le16(dot, 24).unwrap_or(0)) << 16)
                | u32::from(le16(dot, 22).unwrap_or(0)),
            utc_offset: 0,
        };
        Some(Look::Head {
            ended: has_end_marker(bytes),
            entries: u32::try_from(entries).unwrap_or(u32::MAX),
            modified: modified.text().is_some().then_some(modified),
        })
    }

    /// 整簇都是格式正确的目录项（[`short_entry_ok`]、[`lfn_entry_ok`]）；结束标记之后全为 0。
    /// 只有一项时证据不足，只能接在相邻的目录内容后面。
    fn look_body(&self, bytes: &[u8]) -> Look {
        let mut entries = 0u32;
        let mut ended = false;
        for raw in bytes.as_chunks::<ENTRY>().0 {
            if ended || raw[0] == 0 {
                // 新分配的目录簇是清零过的：结束标记之后不会有别的东西
                if raw.iter().any(|&byte| byte != 0) {
                    return Look::Other;
                }
                ended = true;
                continue;
            }
            let valid = if raw[11] & 0x3F == ATTR_LONG_NAME {
                lfn_entry_ok(raw)
            } else {
                entries = entries.saturating_add(1);
                self.short_entry_ok(raw)
            };
            if !valid {
                return Look::Other;
            }
        }
        if entries == 0 {
            return Look::Other;
        }
        Look::Body {
            ended,
            entries,
            strong: entries >= 2,
        }
    }

    /// 一个短文件名项像不像系统写出来的：
    /// - 属性的高两位为 0，不是卷标；大小写标记（第 12 字节）只用 0x08、0x10 两位；
    /// - 名字：首字节是删除标记、0x05 或合法字符，其余是合法字符（不含小写、控制字符、`.` 与规范禁用的符号），
    ///   基本名与扩展名里空格只在末尾；
    /// - 修改时间合法；创建时间、访问日期为 0 或合法，创建时间的 10 毫秒数不超过 199；
    /// - 目录长度为 0、首簇合法；空文件首簇为 0；其余文件首簇合法、长度装得进簇区。
    fn short_entry_ok(&self, raw: &[u8; ENTRY]) -> bool {
        let attr = raw[11];
        if attr & 0xC0 != 0 || attr & ATTR_VOLUME_ID != 0 || raw[12] & !0x18 != 0 {
            return false;
        }
        let Some(name) = raw.first_chunk::<11>() else {
            return false;
        };
        let (base, ext) = name.split_at(8);
        let first_ok = matches!(name[0], DELETED | KANJI_E5) || name_byte_ok(name[0]);
        if !first_ok
            || !name
                .iter()
                .skip(1)
                .all(|&byte| byte == b' ' || name_byte_ok(byte))
            || !padded_at_end(base)
            || !padded_at_end(ext)
        {
            return false;
        }
        let word = |at| le16(raw, at).unwrap_or(0);
        let optional_time =
            |date: u16, time: u16| (date == 0 && time == 0) || dos_time(date, time).is_some();
        if dos_time(word(24), word(22)).is_none()
            || !optional_time(word(16), word(14))
            || raw[13] > 199
            || !optional_time(word(18), 0)
        {
            return false;
        }
        let first = fat_first_cluster(raw, self.kind == FsKind::Fat32);
        let size = u64::from(le32(raw, 28).unwrap_or(0));
        if attr & ATTR_DIRECTORY != 0 {
            size == 0 && self.clusters.is_valid(first)
        } else if size == 0 {
            first == 0
        } else {
            self.clusters.is_valid(first)
                && self.clusters.clusters_for(size) <= u64::from(self.clusters.count)
        }
    }
}

impl FreeSpace for Fat {
    fn clusters(&self) -> &Clusters {
        &self.clusters
    }

    fn root(&self) -> FileRef {
        Fat::root(self)
    }

    fn list_runs(&self, dir: &FileRef) -> Result<(Listing, Vec<Run>), FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        if dir.is_orphans() {
            return Ok((self.scan.listing(&self.clusters), Vec::new()));
        }
        let (bytes, runs, overwritten) = self.dir_contents(dir)?;
        let raw = parse_dir(&bytes, dir.deleted, self.kind == FsKind::Fat32);
        let mut entries = Vec::with_capacity(raw.entries.len());
        for item in raw.entries {
            entries.push(self.entry(item)?);
        }
        if self.is_root(dir)
            && let Some(orphans) = self.scan.root_entry()
        {
            entries.push(orphans);
        }
        let listing = Listing {
            entries,
            truncated: raw.truncated,
            overwritten,
        };
        Ok((listing, runs))
    }

    fn is_free(&self, cluster: u32) -> Result<bool, FsError> {
        Ok(self.clusters.link(cluster)? == Link::Free)
    }

    fn look(&self, cluster: u32, bytes: &[u8]) -> Look {
        self.look_head(cluster, bytes)
            .unwrap_or_else(|| self.look_body(bytes))
    }

    fn subdirs(&self, bytes: &[u8]) -> Vec<Run> {
        let is_fat32 = self.kind == FsKind::Fat32;
        bytes
            .as_chunks::<ENTRY>()
            .0
            .iter()
            .take_while(|raw| raw[0] != 0)
            .filter(|raw| {
                let attr = raw[11];
                attr & 0x3F != ATTR_LONG_NAME
                    && attr & ATTR_DIRECTORY != 0
                    && attr & ATTR_VOLUME_ID == 0
                    && !is_dot_entry(raw)
            })
            .map(|raw| fat_first_cluster(raw, is_fat32))
            .filter(|&first| self.clusters.is_valid(first))
            .map(|first| Run { first, count: 1 })
            .collect()
    }
}

/// `.` 或 `..` 项。
fn is_dot_entry(raw: &[u8; ENTRY]) -> bool {
    matches!(
        raw.first_chunk::<11>(),
        Some(b".          " | b"..         ")
    )
}

/// 短文件名里允许的字符（首字节另有删除标记与 0x05 两种写法）：OEM 代码页的字节，或可打印、非小写、
/// 不是 `.`、不是规范禁用的符号。
fn name_byte_ok(byte: u8) -> bool {
    byte >= 0x80 || (plausible_first_byte(byte) && byte != 0x7F)
}

/// 名字里的空格只用来补齐末尾。
fn padded_at_end(part: &[u8]) -> bool {
    part.iter()
        .skip_while(|&&byte| byte != b' ')
        .all(|&byte| byte == b' ')
}

/// 一个长文件名项像不像系统写出来的：属性正好是 0x0F，类型（第 12 字节）与首簇（第 26、27 字节）为 0，
/// 序号是删除标记或 1–20（只可另带「最后一段」的 0x40）；名字里没有控制字符，结束的 0 之后全是 `0xFFFF` 填充。
fn lfn_entry_ok(raw: &[u8; ENTRY]) -> bool {
    let order = raw[0];
    let order_ok = order == DELETED || (order & 0xA0 == 0 && (1..=20).contains(&(order & 0x1F)));
    if !order_ok || raw[11] != ATTR_LONG_NAME || raw[12] != 0 || raw[26] != 0 || raw[27] != 0 {
        return false;
    }
    let Some(part) = lfn_part(raw) else {
        return false;
    };
    let mut ended = false;
    for unit in part.units {
        let ok = if ended {
            unit == 0xFFFF
        } else if unit == 0 {
            ended = true;
            true
        } else {
            unit >= 0x20 && unit != 0xFFFF
        };
        if !ok {
            return false;
        }
    }
    true
}

/// 子目录的第一个簇以 `.` 与 `..` 两项开头。
fn starts_with_dots(bytes: &[u8]) -> bool {
    bytes.get(..11) == Some(b".          ".as_slice())
        && bytes.get(32..43) == Some(b"..         ".as_slice())
}

/// 目录里有没有「此后再无目录项」的标记（首字节为 0）。
fn has_end_marker(bytes: &[u8]) -> bool {
    bytes
        .as_chunks::<ENTRY>()
        .0
        .iter()
        .any(|entry| entry[0] == 0)
}

/// 解析出、还没判断可恢复性的目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawEntry {
    name: String,
    short_name: String,
    name_uncertain: bool,
    is_dir: bool,
    deleted: bool,
    first_cluster: u32,
    cluster_high: u16,
    size: u64,
    modified: Stamp,
    created: Stamp,
    attr: u8,
}

struct RawListing {
    entries: Vec<RawEntry>,
    truncated: bool,
}

/// 解析目录内容。`parent_deleted` 为真时，所在目录已被删除，里面的项一律按已删除处理（簇链也已释放）。
fn parse_dir(bytes: &[u8], parent_deleted: bool, is_fat32: bool) -> RawListing {
    let mut entries = Vec::new();
    let mut pending: Vec<LfnPart> = Vec::new();
    for raw in bytes.as_chunks::<ENTRY>().0 {
        let (Some(&first), Some(&attr)) = (raw.first(), raw.get(11)) else {
            break;
        };
        if first == 0 {
            break;
        }
        if attr & 0x3F == ATTR_LONG_NAME {
            if let Some(part) = lfn_part(raw) {
                if first & 0x40 != 0 && first != DELETED {
                    pending.clear();
                }
                pending.push(part);
                if pending.len() > MAX_LFN_PARTS {
                    pending.clear();
                }
            }
            continue;
        }
        let parts = std::mem::take(&mut pending);
        if attr & ATTR_VOLUME_ID != 0 || attr & 0xC0 != 0 {
            continue;
        }
        let Some(short) = raw.get(..11) else {
            continue;
        };
        if short == b".          " || short == b"..         " {
            continue;
        }
        let deleted_here = first == DELETED;
        if deleted_here && !plausible_short(short) {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            return RawListing {
                entries,
                truncated: true,
            };
        }
        entries.push(raw_entry(
            raw,
            attr,
            deleted_here,
            parent_deleted,
            is_fat32,
            &parts,
        ));
    }
    RawListing {
        entries,
        truncated: false,
    }
}

fn raw_entry(
    raw: &[u8],
    attr: u8,
    deleted_here: bool,
    parent_deleted: bool,
    is_fat32: bool,
    parts: &[LfnPart],
) -> RawEntry {
    let mut short: [u8; 11] = raw
        .get(..11)
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([b' '; 11]);
    if short[0] == KANJI_E5 {
        short[0] = DELETED;
    }
    let mut long = None;
    let mut name_uncertain = false;
    if deleted_here {
        // 删除时首字节被改成 0xE5，原来的首字母丢了。有长文件名时按校验和反推，否则用 `_` 代替
        let recovered = parts
            .first()
            .and_then(|part| recover_first_byte(&short, part.checksum))
            .filter(|&byte| plausible_first_byte(byte));
        match recovered {
            Some(byte) if lfn_consistent(parts, true) => {
                short[0] = byte;
                long = lfn_name(parts);
            }
            _ => {
                short[0] = b'_';
                name_uncertain = true;
            }
        }
    } else if lfn_consistent(parts, false)
        && parts.first().map(|part| part.checksum) == Some(short_checksum(&short))
    {
        long = lfn_name(parts);
    }
    let (short_name, non_ascii) = short_display(&short, raw.get(12).copied().unwrap_or(0));
    if long.is_none() && non_ascii {
        name_uncertain = true;
    }
    // FAT12/16 的第 20 字节另有用途，首簇只在 FAT32 上带高 16 位
    let cluster_high = if is_fat32 {
        le16(raw, 20).unwrap_or(0)
    } else {
        0
    };
    let date_time = |date_at, time_at| Stamp::Dos {
        stamp: (u32::from(le16(raw, date_at).unwrap_or(0)) << 16)
            | u32::from(le16(raw, time_at).unwrap_or(0)),
        utc_offset: 0,
    };
    RawEntry {
        name: long.unwrap_or_else(|| short_name.clone()),
        short_name,
        name_uncertain,
        is_dir: attr & ATTR_DIRECTORY != 0,
        deleted: deleted_here || parent_deleted,
        first_cluster: fat_first_cluster(raw, is_fat32),
        cluster_high,
        size: u64::from(le32(raw, 28).unwrap_or(0)),
        modified: date_time(24, 22),
        created: date_time(16, 14),
        attr,
    }
}

/// 长文件名的各段是否属于同一个短文件名：校验和一致、段数合理；未删除的还要序号从 `0x40 | n` 递减到 1。
fn lfn_consistent(parts: &[LfnPart], deleted: bool) -> bool {
    let Some(first) = parts.first() else {
        return false;
    };
    if parts.iter().any(|part| part.checksum != first.checksum) {
        return false;
    }
    if deleted {
        return parts.iter().all(|part| part.order == DELETED);
    }
    lfn_ordered(parts)
}

/// 短文件名（11 字节）的校验和，长文件名的每一段都记着它。
pub fn short_checksum(short: &[u8; 11]) -> u8 {
    short
        .iter()
        .fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

/// 首字节换成多少，短文件名的校验和才等于 `checksum`。校验和对首字节是一一对应的，答案唯一。
fn recover_first_byte(short: &[u8; 11], checksum: u8) -> Option<u8> {
    let mut candidate = *short;
    (0..=u8::MAX).find(|&byte| {
        candidate[0] = byte;
        short_checksum(&candidate) == checksum
    })
}

/// 短文件名里允许出现的首字节：可打印、非小写、不是规范禁止的符号。
fn plausible_first_byte(byte: u8) -> bool {
    byte >= 0x80 || (byte > 0x20 && !byte.is_ascii_lowercase() && !forbidden(byte) && byte != b'.')
}

fn forbidden(byte: u8) -> bool {
    b"\"*+,/:;<=>?[\\]|".contains(&byte)
}

/// 已删除目录项的短文件名是否像样：首字节之后没有控制字符、小写字母、禁用符号。用来过滤已删除目录里的垃圾。
fn plausible_short(short: &[u8]) -> bool {
    short.iter().skip(1).all(|&byte| {
        byte >= 0x80 || (byte >= 0x20 && !byte.is_ascii_lowercase() && !forbidden(byte))
    })
}

/// 8.3 名字显示成 `NAME.EXT`：按大小写标记（目录项第 12 字节：0x08 名字小写、0x10 扩展名小写）转小写；
/// 首字节 0x05 表示它实际是 0xE5；非 ASCII 字节（OEM 代码页）与控制字符显示为 `_`。第二项为有没有非 ASCII 字节。
pub fn short_display(short: &[u8; 11], case_flags: u8) -> (String, bool) {
    let mut non_ascii = false;
    let mut part = |bytes: &[u8], first: bool, lower: bool| -> String {
        let length = bytes
            .iter()
            .rposition(|&byte| byte != b' ')
            .map_or(0, |end| end + 1);
        bytes
            .iter()
            .take(length)
            .enumerate()
            .map(|(index, &byte)| {
                let byte = if first && index == 0 && byte == KANJI_E5 {
                    DELETED
                } else {
                    byte
                };
                if byte >= 0x80 {
                    non_ascii = true;
                    '_'
                } else if !byte.is_ascii_graphic() && byte != b' ' {
                    '_'
                } else if lower {
                    char::from(byte.to_ascii_lowercase())
                } else {
                    char::from(byte)
                }
            })
            .collect()
    };
    let (base, ext) = short.split_at(8);
    let base = part(base, true, case_flags & 0x08 != 0);
    let ext = part(ext, false, case_flags & 0x10 != 0);
    let name = if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    };
    (name, non_ascii)
}

/// 根目录里未删除的卷标项。
fn root_label(bytes: &[u8]) -> Option<String> {
    for raw in bytes.as_chunks::<ENTRY>().0 {
        let (&first, &attr) = (raw.first()?, raw.get(11)?);
        if first == 0 {
            return None;
        }
        if first != DELETED && attr & 0x3F == ATTR_VOLUME_ID {
            return bpb::short_label(raw.get(..11)?);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_checksum_matches_the_specification_example() {
        // 规范里的算法：sum = ((sum & 1) << 7) + (sum >> 1) + byte
        let short = *b"FOO     BAR";
        let mut sum = 0u8;
        for byte in short {
            sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
        }
        assert_eq!(short_checksum(&short), sum);
    }

    #[test]
    fn first_byte_is_recovered_from_the_checksum() {
        let short = *b"REPORT~1DOC";
        let checksum = short_checksum(&short);
        let mut deleted = short;
        deleted[0] = DELETED;
        assert_eq!(recover_first_byte(&deleted, checksum), Some(b'R'));
    }

    #[test]
    fn short_names_respect_case_flags() {
        assert_eq!(short_display(b"README  TXT", 0x08).0, "readme.TXT");
        assert_eq!(short_display(b"NOEXT      ", 0).0, "NOEXT");
        assert_eq!(
            short_display(b"\x05BC     TXT", 0x10),
            ("_BC.txt".to_owned(), true)
        );
        assert_eq!(short_display(b"A\x01B     TXT", 0).0, "A_B.TXT");
        assert_eq!(
            short_display(
                &[
                    0xC4, b'A', b' ', b' ', b' ', b' ', b' ', b' ', b'T', b'X', b'T'
                ],
                0
            ),
            ("_A.TXT".to_owned(), true)
        );
    }
}
