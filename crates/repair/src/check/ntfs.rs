//! NTFS 只做有把握的几项：引导扇区的参数、备份引导扇区（分区最后一个扇区）、`$MFT` 与 `$MFTMirr` 的位置、
//! `$MFT` 第一条记录、`$MFTMirr` 与 `$MFT` 前几条记录是否一致。

use yisi_fs::bpb::{self, NtfsBpb as Geometry, Problem};
use yisi_part::bytes::le16;

use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::{CheckItem, Ctx, FsType, Stage, Summary};

/// `$MFTMirr` 至少镜像前 4 条记录（`$MFT`、`$MFTMirr`、`$LogFile`、`$Volume`）。
const MIRRORED_RECORDS: u64 = 4;

/// 解析引导扇区：规则在 [`yisi_fs::bpb`]（与打开卷、引导扇区诊断共用）。有致命问题时参数为 `None`。
fn parse(sector: &[u8]) -> (Option<Geometry>, Vec<Problem>) {
    let checked = bpb::check_ntfs(sector);
    (checked.geometry, checked.problems)
}

pub(crate) fn looks_valid(sector: &[u8]) -> bool {
    parse(sector).0.is_some()
}

/// 检查一个 NTFS 卷。被取消时返回 `None`。
pub(crate) fn check(ctx: &mut Ctx, sector: &[u8], from_backup: bool) -> Option<Summary> {
    let (geometry, problems) = parse(sector);
    ctx.boot_problems(&problems, Area::Boot);
    let Some(geometry) = geometry else {
        return Some(summary(FsType::Ntfs, 0, 0));
    };
    ctx.checked(CheckItem::Boot);
    let length = ctx.disk.length();
    let bps = geometry.bytes_per_sector;
    let volume_bytes = geometry.total_sectors.saturating_mul(bps);
    if volume_bytes > length {
        ctx.issues.push(
            Issue::new(IssueKind::VolumeTruncated)
                .area(Area::Boot)
                .expected(volume_bytes)
                .actual(length),
        );
    }
    if !from_backup {
        check_backup(ctx, &geometry, sector);
    }
    let clusters = volume_bytes / geometry.cluster_size;
    let summary = summary(FsType::Ntfs, geometry.cluster_size, clusters);
    if !ctx.control.checkpoint() {
        return None;
    }

    let records = MIRRORED_RECORDS * geometry.record_size;
    ctx.control.progress(Stage::Mft, 0, 2);
    let Some(mft) = read_records(ctx, &geometry, geometry.mft_lcn, records, Area::Mft) else {
        ctx.skipped(CheckItem::MftMirror);
        return Some(summary);
    };
    ctx.checked(CheckItem::Mft);
    let first = mft.get(..geometry.record_size as usize).unwrap_or_default();
    // 第一条记录就是 $MFT 自己：签名是 FILE，并且在用
    let in_use = le16(first, 22).is_some_and(|flags| flags & 0x01 != 0);
    if first.get(..4) != Some(b"FILE".as_slice()) || !in_use {
        ctx.issues
            .push(Issue::new(IssueKind::MftInvalid).area(Area::Mft));
    }
    ctx.control.progress(Stage::Mft, 1, 2);
    if !ctx.control.checkpoint() {
        return None;
    }
    if let Some(mirror) = read_records(
        ctx,
        &geometry,
        geometry.mirror_lcn,
        records,
        Area::MftMirror,
    ) {
        ctx.checked(CheckItem::MftMirror);
        let record = geometry.record_size as usize;
        let differing = mft
            .chunks(record)
            .zip(mirror.chunks(record))
            .filter(|(a, b)| a != b)
            .count();
        if differing > 0 {
            ctx.issues.push(
                Issue::new(IssueKind::MftMirrorMismatch)
                    .area(Area::MftMirror)
                    .count(differing as u64),
            );
        }
    } else {
        ctx.skipped(CheckItem::MftMirror);
    }
    ctx.control.progress(Stage::Mft, 2, 2);
    Some(summary)
}

fn summary(fs: FsType, cluster_size: u64, cluster_count: u64) -> Summary {
    Summary {
        fs,
        cluster_size,
        cluster_count,
        used_clusters: 0,
        files: 0,
        directories: 0,
    }
}

/// 从簇号 `lcn` 起读 `length` 字节；位置在卷外、读不出时记下问题，返回 `None`。
fn read_records(
    ctx: &mut Ctx,
    geometry: &Geometry,
    lcn: u64,
    length: u64,
    area: Area,
) -> Option<Vec<u8>> {
    let volume_bytes = geometry
        .total_sectors
        .saturating_mul(geometry.bytes_per_sector);
    let start = lcn.checked_mul(geometry.cluster_size);
    let Some(start) = start.filter(|&start| {
        start
            .checked_add(length)
            .is_some_and(|end| end <= volume_bytes && end <= ctx.disk.length())
    }) else {
        ctx.issues
            .push(Issue::new(IssueKind::MftOutOfRange).area(area).actual(lcn));
        return None;
    };
    let mut data = vec![0u8; length as usize];
    let bad = ctx.read(start, &mut data, area, None)?;
    bad.is_empty().then_some(data)
}

/// 备份引导扇区在 BPB 记录的扇区数处（分区最后一个扇区）：应能认出，参数应与主引导扇区一致。
fn check_backup(ctx: &mut Ctx, geometry: &Geometry, main: &[u8]) {
    ctx.checked(CheckItem::BackupBoot);
    let at = geometry
        .total_sectors
        .saturating_mul(geometry.bytes_per_sector);
    let mut backup = vec![0u8; 512];
    let readable = ctx
        .read(at, &mut backup, Area::BackupBoot, None)
        .is_some_and(|bad| bad.is_empty());
    if !readable || parse(&backup).0.is_none() {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootInvalid).area(Area::BackupBoot));
        return;
    }
    // 比到卷序列号为止（0x0B 到 0x50）
    if backup.get(0x0B..0x50) != main.get(0x0B..0x50) {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootMismatch).area(Area::BackupBoot));
    }
}
