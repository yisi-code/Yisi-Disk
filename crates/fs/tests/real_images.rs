//! 对照 Windows 自己格式化、写入、删除过的镜像（由 `make-test-images.ps1` 生成）：
//! 按清单逐个找到文件，未删除的内容必须一致，已删除的报告判断结果与内容是否一致。
//!
//! 手动运行（PowerShell）：
//! `$env:YISI_FS_TEST_IMAGES = '<镜像目录>'; cargo test -p yisi-fs --test real_images -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use yisi_fs::{Entry, FileRef, State, Volume};
use yisi_io::{BlockDevice, RawImage};

struct Expected {
    label: String,
    path: String,
    length: u64,
    sha: String,
    kind: String,
}

#[test]
#[ignore = "需要先用 make-test-images.ps1 生成镜像，并设置 YISI_FS_TEST_IMAGES"]
fn windows_formatted_images_match_their_manifests() {
    let dir = std::env::var("YISI_FS_TEST_IMAGES").expect("设置 YISI_FS_TEST_IMAGES 为镜像目录");
    let mut images: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "vhd"))
        .collect();
    images.sort();
    assert!(!images.is_empty(), "{dir} 里没有 .vhd");
    let mut failures = Vec::new();
    for image in images {
        check_image(&image, &mut failures);
    }
    assert!(failures.is_empty(), "不一致：\n{}", failures.join("\n"));
}

fn check_image(image: &Path, failures: &mut Vec<String>) {
    println!("== {}", image.display());
    let device: Arc<dyn BlockDevice> = Arc::new(RawImage::open(image, 512).unwrap());
    let table = yisi_part::read_table(device.as_ref()).unwrap();
    println!("分区表 {:?}，说明 {:?}", table.scheme, table.notes);
    let mut volumes = BTreeMap::new();
    for partition in &table.partitions {
        let kind = yisi_fs::detect(device.as_ref(), partition.offset, partition.length).unwrap();
        println!(
            "  分区 {}：偏移 {}，长度 {}，{:?}",
            partition.number, partition.offset, partition.length, kind
        );
        if let Ok(volume) = Volume::open(Arc::clone(&device), partition.offset, partition.length) {
            let label = volume.label().unwrap_or_default().to_owned();
            println!("    卷标 {label}，簇 {} 字节", volume.cluster_size());
            volumes.insert(label, volume);
        }
    }

    let manifest = fs::read_to_string(image.with_extension("tsv")).unwrap();
    for line in manifest.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        let expected = Expected {
            label: fields[0].to_owned(),
            path: fields[1].to_owned(),
            length: fields[2].parse().unwrap(),
            sha: fields[3].to_owned(),
            kind: fields[4].to_owned(),
        };
        let Some(volume) = volumes.get(&expected.label) else {
            failures.push(format!("{}：找不到卷 {}", image.display(), expected.label));
            continue;
        };
        check_file(volume, &expected, failures);
    }
}

fn check_file(volume: &Volume, expected: &Expected, failures: &mut Vec<String>) {
    let want_deleted = expected.kind.starts_with("deleted");
    let shown = format!("{}:{}", expected.label, expected.path);
    let Some(entry) = locate(volume, &expected.path, want_deleted) else {
        failures.push(format!("{shown}：没找到"));
        return;
    };
    let content = read_all(volume, &entry.file);
    let matches = content
        .as_ref()
        .is_ok_and(|data| data.len() as u64 == expected.length && sha256_hex(data) == expected.sha);
    if !expected.path.starts_with("many/") || !matches {
        println!(
            "  {shown:<90} {:<20} {:?} 名字 {:?}{} 内容{}",
            expected.kind,
            entry.state,
            entry.name,
            if entry.name_uncertain {
                "（不确定）"
            } else {
                ""
            },
            match &content {
                Ok(_) if matches => "一致".to_owned(),
                Ok(data) => format!("不一致（读出 {} 字节）", data.len()),
                Err(error) => format!("读不出：{error}"),
            }
        );
    }
    if entry.deleted() != want_deleted {
        failures.push(format!("{shown}：删除标记为 {}", entry.deleted()));
    }
    if entry.size != expected.length {
        failures.push(format!(
            "{shown}：长度 {} 应为 {}",
            entry.size, expected.length
        ));
    }
    let must_match = match expected.kind.as_str() {
        "live" | "live-fragmented" => true,
        // 已删除且判为「可能完整」的连续文件，内容应当对得上
        "deleted" | "deleted-shortname" => entry.state == State::Intact,
        // 碎片文件删除后：FAT 的簇链已清零，读不对是已知局限；exFAT 的 FAT 链还在，应当对得上
        _ => false,
    };
    if must_match && !matches {
        failures.push(format!("{shown}：内容不一致（{:?}）", entry.state));
    }
}

/// 按路径一级级找下去。已删除的短文件名丢了首字母，按其余部分匹配。
fn locate(volume: &Volume, path: &str, want_deleted: bool) -> Option<Entry> {
    let mut dir = volume.root();
    let parts: Vec<&str> = path.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        let entries = volume.list(&dir).ok()?.entries;
        let is_last = index + 1 == parts.len();
        let candidates: Vec<&Entry> = entries
            .iter()
            .filter(|entry| {
                entry.name == *part
                    || (entry.name_uncertain
                        && entry.deleted()
                        && entry.name.get(1..) == part.get(1..))
            })
            .collect();
        let chosen = candidates
            .iter()
            .find(|entry| !is_last || entry.deleted() == want_deleted)
            .or_else(|| candidates.first())?;
        if is_last {
            return Some((*chosen).clone());
        }
        dir = chosen.file;
    }
    None
}

fn read_all(volume: &Volume, file: &FileRef) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    volume
        .open_file(file)
        .map_err(|error| error.to_string())?
        .read_to_end(&mut data)
        .map_err(|error| error.to_string())?;
    Ok(data)
}

/// 只给测试核对内容用的 SHA-256（FIPS 180-4）。
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (index, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes(*word);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[test]
fn sha256_matches_known_digests() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
