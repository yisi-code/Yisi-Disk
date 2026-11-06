//! 程序内更新：只在用户点「检查新版本」「下载并更新」时联网，不自动检查、不在后台下载。
//!
//! 发布源有两个：GitHub 与它在 Gitee 上的镜像（有些地区连 GitHub 很慢）。两边同时问，取最新的版本，同一版本按
//! 答复快慢排先后；下载太慢（估计还要一分钟以上）或出错时换下一个源。两边下来的东西都过同一套签名核对。
//!
//! 流程：问最新发布 → 下载签名清单（`.sig`），用内置公钥核对签名、清单里的版本必须比当前新 →
//! 下载压缩包，大小与 SHA256 必须与清单一致（GitHub 给出的摘要也要一致）→ 解到程序文件夹的 `data/update`，
//! 只收程序包里那几个已知的文件 → 把旧的程序文件改名为 `.old`、换上新的，任何一步失败都改回去 →
//! 重启：新进程等旧进程退出后再建窗口，然后删掉 `.old` 与解压目录。
//!
//! 签名用 ECDSA P-256（SHA-256），私钥只在发版的机器上，程序里只有公钥；没有签名文件、签名不对的版本一律不装。

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::engines::{HostError, data_dir, program_dir};

/// GitHub：查最新发布的地址；资产只认这个前缀下的下载地址（会再跳转到它的存储，WinHTTP 照常跟随，但不许从
/// https 跳到 http）；发布页。
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/yisi-code/Yisi-Disk/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/yisi-code/Yisi-Disk/releases/download/";
const GITHUB_PAGE: &str = "https://github.com/yisi-code/Yisi-Disk/releases/latest";
/// Gitee 镜像：同样的三项。
const GITEE_LATEST_API: &str = "https://gitee.com/api/v5/repos/yisi-code/yisi-disk/releases/latest";
const GITEE_DOWNLOAD_PREFIX: &str = "https://gitee.com/yisi-code/yisi-disk/releases/download/";
const GITEE_PAGE: &str = "https://gitee.com/yisi-code/yisi-disk/releases";
/// 下载满这么久之后，按当前速度估计剩下的还要超过 `SLOW_REMAINING` 就换下一个源；这么久一个字节都没收到也换。
const SLOW_AFTER: Duration = Duration::from_secs(8);
const SLOW_REMAINING: Duration = Duration::from_secs(60);
const STALLED_AFTER: Duration = Duration::from_secs(15);
/// 本机走查用：设为 `http://127.0.0.1:<端口>/...`（可以逗号分隔写几个）时改问本机的假发布源，资产也只认各自
/// 同一个源。签名照样要核对，所以它只能装上用私钥签过的包。
const LOCAL_API_ENV: &str = "YISI_UPDATE_API";
const LOCAL_ORIGIN_PREFIX: &str = "http://127.0.0.1:";

/// 发版签名的公钥（P-256 曲线上的点，X、Y 各 32 字节，大端）。私钥不在仓库里。
const PUBLIC_KEY: [u8; 64] = [
    0x67, 0x09, 0x38, 0x94, 0xf0, 0x13, 0x2b, 0x45, 0x90, 0x73, 0x16, 0x6b, 0x6e, 0x6d, 0x93, 0xe6,
    0xf6, 0xbd, 0x2d, 0xf7, 0x6f, 0xb6, 0x83, 0xb1, 0xf5, 0x61, 0x8f, 0x81, 0xbe, 0xc1, 0x11, 0x91,
    0xd0, 0xc1, 0x5f, 0xb6, 0xf9, 0xef, 0x24, 0x52, 0x88, 0x5c, 0x98, 0xfc, 0x05, 0x92, 0x3a, 0xb3,
    0xaf, 0x8d, 0x16, 0xed, 0x7a, 0x83, 0xad, 0x50, 0x18, 0x85, 0x59, 0xda, 0xae, 0xc1, 0x13, 0x27,
];

/// 签名清单的第一行，格式变了就换版本号。
const MANIFEST_HEADER: &str = "yisi-disk-tools update v1";
const MAX_API_BYTES: usize = 1 << 20;
const MAX_SIGNATURE_BYTES: usize = 4 << 10;
const MAX_ARCHIVE_BYTES: u64 = 256 << 20;
const MAX_ENTRIES: usize = 64;

/// 程序包里的主程序名；装到本机时换成当前运行的程序文件（开发构建叫 `yisi-app.exe`）。
const PACKAGE_MAIN_EXE: &str = "yisi-磁盘工具.exe";
/// 程序包里允许出现、要装到程序文件夹的文件。前三个必须有。
const PACKAGE_FILES: [&str; 6] = [
    PACKAGE_MAIN_EXE,
    "yisi-engine.exe",
    "yisi-broker.exe",
    "LICENSE.md",
    "THIRD-PARTY-NOTICES.txt",
    "使用说明.txt",
];
const REQUIRED_FILES: usize = 3;
/// 换下来的旧文件加的后缀；下次启动时删掉。
const OLD_SUFFIX: &str = ".old";
/// 新进程启动时带上这个参数和旧进程的 PID，先等旧进程退出。
pub const AFTER_UPDATE_ARG: &str = "--after-update";

/// 同一时刻只做一次更新。
static INSTALLING: AtomicBool = AtomicBool::new(false);

/// 更新的进度：`phase` 为 `download`（下载新版本，`done` / `total` 是字节数）或 `install`（安装新版本：
/// `step` 依次为 `verify` 核对签名与摘要、`extract` 解压、`replace` 换上，`done` / `total` 是这一步的件数）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    pub phase: &'static str,
    pub step: &'static str,
    pub done: u64,
    pub total: u64,
    /// 从哪个源下载的（`github`、`gitee`、本机走查时 `local`）。
    pub source: &'static str,
}

impl UpdateProgress {
    fn download(source: &'static str, done: u64, total: u64) -> Self {
        Self {
            phase: "download",
            step: "archive",
            done,
            total,
            source,
        }
    }

    fn install(source: &'static str, step: &'static str, done: usize, total: usize) -> Self {
        Self {
            phase: "install",
            step,
            done: done as u64,
            total: total as u64,
            source,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub available: bool,
    /// 有最新版本、答复最快的源，下载时先从它下。
    pub source: &'static str,
}

/// 比较两个版本号（`v0.2.0`、`0.10.1`，只比数字段；`-beta` 之类的后缀不参与）。
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |version: &str| -> Vec<u64> {
        version
            .trim_start_matches(['v', 'V'])
            .split('-')
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    let (left, right) = (parts(a), parts(b));
    for index in 0..left.len().max(right.len()) {
        let ordering = left
            .get(index)
            .unwrap_or(&0)
            .cmp(right.get(index).unwrap_or(&0));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// 发布源：GitHub、Gitee 镜像，或本机走查用的假源（只许 127.0.0.1，设了它就只用它）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    name: &'static str,
    api: String,
    /// 资产下载地址必须以它开头。
    asset_prefix: String,
}

impl Source {
    fn github() -> Self {
        Self {
            name: "github",
            api: LATEST_RELEASE_API.to_owned(),
            asset_prefix: DOWNLOAD_PREFIX.to_owned(),
        }
    }

    fn gitee() -> Self {
        Self {
            name: "gitee",
            api: GITEE_LATEST_API.to_owned(),
            asset_prefix: GITEE_DOWNLOAD_PREFIX.to_owned(),
        }
    }

    fn all_from_env() -> Vec<Self> {
        let local = std::env::var(LOCAL_API_ENV)
            .map(|value| Self::local(&value))
            .unwrap_or_default();
        if local.is_empty() {
            vec![Self::github(), Self::gitee()]
        } else {
            local
        }
    }

    /// 本机走查用的假源：逗号分隔的几个 `http://127.0.0.1:<端口>/…`（最多三个，用来走查换源），别的地址不认。
    fn local(value: &str) -> Vec<Self> {
        const NAMES: [&str; 3] = ["local", "local-2", "local-3"];
        value
            .split(',')
            .map(str::trim)
            .filter(|api| api.starts_with(LOCAL_ORIGIN_PREFIX))
            .zip(NAMES)
            .map(|(api, name)| {
                let origin_end = api[LOCAL_ORIGIN_PREFIX.len()..]
                    .find('/')
                    .map_or(api.len(), |at| LOCAL_ORIGIN_PREFIX.len() + at);
                Self {
                    name,
                    asset_prefix: format!("{}/", &api[..origin_end]),
                    api: api.to_owned(),
                }
            })
            .collect()
    }
}

/// 发布页：`gitee` 时打开 Gitee 镜像的，其余打开 GitHub 的。网址都写死在这里。
pub fn releases_page(source: Option<&str>) -> &'static str {
    if source == Some("gitee") {
        GITEE_PAGE
    } else {
        GITHUB_PAGE
    }
}

/// 一个源的答复：发布内容（或错误）与用时。
struct Answer {
    source: Source,
    release: Result<Release, HostError>,
    elapsed: Duration,
}

/// 同时问各个源的最新发布。
fn ask_all(sources: Vec<Source>) -> Vec<Answer> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = sources
            .into_iter()
            .map(|source| {
                scope.spawn(move || {
                    let started = Instant::now();
                    let release = http_get(&source.api, MAX_API_BYTES, true, &mut |_| true)
                        .and_then(|json| parse_release(&json, &source));
                    Answer {
                        source,
                        release,
                        elapsed: started.elapsed(),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .collect()
    })
}

/// 取各源里最新的版本；有这个版本的源按答复快慢排先后。都失败时把各源的错误合在一起报。
fn rank(answers: Vec<Answer>) -> Result<Vec<(Source, Release)>, HostError> {
    let mut failures = Vec::new();
    let mut ok: Vec<(Source, Release, Duration)> = Vec::new();
    for answer in answers {
        match answer.release {
            Ok(release) => ok.push((answer.source, release, answer.elapsed)),
            Err(error) => failures.push((answer.source.name, error)),
        }
    }
    let Some(latest) = ok
        .iter()
        .map(|(_, release, _)| release.version.clone())
        .max_by(|a, b| compare_versions(a, b))
    else {
        let detail = failures
            .iter()
            .map(|(name, error)| format!("{name}：{}", error.detail))
            .collect::<Vec<_>>()
            .join("；");
        let code = failures
            .first()
            .map_or("updateNetwork", |(_, error)| error.code);
        return Err(HostError::new(code, detail));
    };
    ok.retain(|(_, release, _)| compare_versions(&release.version, &latest) == Ordering::Equal);
    ok.sort_by_key(|(_, _, elapsed)| *elapsed);
    Ok(ok
        .into_iter()
        .map(|(source, release, _)| (source, release))
        .collect())
}

/// 下载是不是太慢、该换源了：下了 `SLOW_AFTER` 之后按平均速度估计剩下的超过 `SLOW_REMAINING`，或者
/// `STALLED_AFTER` 还一个字节都没收到。
fn too_slow(elapsed: Duration, received: u64, total: u64) -> bool {
    if received == 0 {
        return elapsed >= STALLED_AFTER;
    }
    if elapsed < SLOW_AFTER || received >= total {
        return false;
    }
    let remaining = (total - received) as f64 * elapsed.as_secs_f64() / received as f64;
    remaining > SLOW_REMAINING.as_secs_f64()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asset {
    url: String,
    size: u64,
    /// GitHub 给出的摘要（`sha256:` 后面的十六进制），没有时为空。
    sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    version: String,
    archive_name: String,
    archive: Option<Asset>,
    signature: Option<Asset>,
}

/// 发布的压缩包名：GitHub 的资产名不能带中文，所以用英文名。
pub fn archive_name(version: &str) -> String {
    format!("yisi-disk-tools-v{version}-windows-x64.zip")
}

/// 程序包里的顶层文件夹名。
fn package_folder(version: &str) -> String {
    format!("yisi-磁盘工具-v{version}-windows-x64")
}

fn parse_release(json: &[u8], source: &Source) -> Result<Release, HostError> {
    let value: serde_json::Value = serde_json::from_slice(json)
        .map_err(|error| HostError::new("updateNoRelease", error.to_string()))?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| HostError::new("updateNoRelease", "tag_name"))?;
    let version = tag.trim_start_matches(['v', 'V']).to_owned();
    let valid = !version.is_empty()
        && version.len() <= 32
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    if !valid {
        return Err(HostError::new("updateNoRelease", tag));
    }
    let archive_name = archive_name(&version);
    let signature_name = format!("{archive_name}.sig");
    let assets = value
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let find = |name: &str| -> Option<Asset> {
        let asset = assets
            .iter()
            .find(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(name))?;
        let url = asset.get("browser_download_url")?.as_str()?;
        if !url.starts_with(&source.asset_prefix) {
            return None;
        }
        Some(Asset {
            url: url.to_owned(),
            size: asset
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            sha256: asset
                .get("digest")
                .and_then(serde_json::Value::as_str)
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .map(str::to_ascii_lowercase),
        })
    };
    Ok(Release {
        archive: find(&archive_name),
        signature: find(&signature_name),
        version,
        archive_name,
    })
}

/// 签名清单：签名覆盖 `signature:` 那行之前的全部字节。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    version: String,
    file: String,
    size: u64,
    sha256: String,
    signed: Vec<u8>,
    signature: [u8; 64],
}

fn parse_manifest(text: &[u8]) -> Result<Manifest, HostError> {
    let bad = |what: &str| HostError::new("updateSignature", what.to_owned());
    let text = std::str::from_utf8(text).map_err(|_| bad("not utf-8"))?;
    let at = text
        .rfind("signature: ")
        .ok_or_else(|| bad("no signature"))?;
    let (body, signature_line) = text.split_at(at);
    let hex = signature_line
        .strip_prefix("signature: ")
        .map(str::trim_end)
        .ok_or_else(|| bad("signature"))?;
    let signature: [u8; 64] = decode_hex(hex)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| bad("signature length"))?;
    let mut lines = body.split('\n');
    if lines.next() != Some(MANIFEST_HEADER) {
        return Err(bad("header"));
    }
    let mut field = |name: &str| -> Result<String, HostError> {
        lines
            .next()
            .and_then(|line| line.strip_prefix(name))
            .and_then(|rest| rest.strip_prefix(": "))
            .map(str::to_owned)
            .ok_or_else(|| bad(name))
    };
    let version = field("version")?;
    let file = field("file")?;
    let size = field("size")?.parse().map_err(|_| bad("size"))?;
    let sha256 = field("sha256")?;
    if sha256.len() != 64 || decode_hex(&sha256).is_none() {
        return Err(bad("sha256"));
    }
    if lines.next() != Some("") || lines.next().is_some() {
        return Err(bad("trailing"));
    }
    Ok(Manifest {
        version,
        file,
        size,
        sha256: sha256.to_ascii_lowercase(),
        signed: body.as_bytes().to_vec(),
        signature,
    })
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 清单签名对、文件名与版本都对得上，且比当前版本新。
fn check_manifest(manifest: &Manifest, release: &Release, current: &str) -> Result<(), HostError> {
    if !verify_signature(&manifest.signed, &manifest.signature, &PUBLIC_KEY)? {
        return Err(HostError::new("updateSignature", "签名不对"));
    }
    if manifest.file != release.archive_name || manifest.version != release.version {
        return Err(HostError::new(
            "updateMismatch",
            format!("{} {}", manifest.file, manifest.version),
        ));
    }
    if compare_versions(&manifest.version, current) != Ordering::Greater {
        return Err(HostError::new("updateNotNewer", manifest.version.clone()));
    }
    if manifest.size == 0 || manifest.size > MAX_ARCHIVE_BYTES {
        return Err(HostError::new("updateMismatch", manifest.size.to_string()));
    }
    Ok(())
}

/// 下载下来的压缩包与清单（以及 GitHub 给出的摘要）一致。
fn check_archive(archive: &[u8], manifest: &Manifest, asset: &Asset) -> Result<(), HostError> {
    let digest = hex(&Sha256::digest(archive));
    if archive.len() as u64 != manifest.size || digest != manifest.sha256 {
        return Err(HostError::new("updateMismatch", digest));
    }
    if asset
        .sha256
        .as_ref()
        .is_some_and(|github| *github != digest)
    {
        return Err(HostError::new("updateMismatch", "GitHub digest"));
    }
    Ok(())
}

/// 从程序包里取出已知的那几个文件（名字与内容）。只认顶层文件夹 `package_folder(version)` 下的这些名字，
/// 别的文件、子目录、绝对路径、`..` 一律拒绝；每个文件核对大小与 CRC-32。
fn extract(zip: &[u8], version: &str) -> Result<Vec<(&'static str, Vec<u8>)>, HostError> {
    use yisi_part::bytes::{le16, le32};
    let bad = |what: String| HostError::new("updateArchive", what);
    let folder = package_folder(version);
    let search_from = zip.len().saturating_sub(22 + 0xffff);
    let end = (search_from..=zip.len().saturating_sub(22))
        .rev()
        .find(|&at| le32(zip, at) == Some(0x0605_4b50))
        .ok_or_else(|| bad("找不到中央目录".into()))?;
    let count = usize::from(le16(zip, end + 10).ok_or_else(|| bad("中央目录".into()))?);
    if count > MAX_ENTRIES {
        return Err(bad(format!("{count} 项")));
    }
    let mut at = le32(zip, end + 16).ok_or_else(|| bad("中央目录".into()))? as usize;
    let mut files = Vec::new();
    for _ in 0..count {
        if le32(zip, at) != Some(0x0201_4b50) {
            return Err(bad(format!("中央目录项 {at}")));
        }
        let field16 =
            |offset: usize| le16(zip, at + offset).ok_or_else(|| bad("中央目录项".into()));
        let field32 =
            |offset: usize| le32(zip, at + offset).ok_or_else(|| bad("中央目录项".into()));
        let (flags, method) = (field16(8)?, field16(10)?);
        let (crc, packed, size) = (field32(16)?, field32(20)? as usize, field32(24)? as usize);
        let (name_len, extra_len, comment_len) = (
            usize::from(field16(28)?),
            usize::from(field16(30)?),
            usize::from(field16(32)?),
        );
        let local = field32(42)? as usize;
        let name_bytes = zip
            .get(at + 46..at + 46 + name_len)
            .ok_or_else(|| bad("名字".into()))?;
        at += 46 + name_len + extra_len + comment_len;
        // 非 ASCII 的名字必须标明是 UTF-8（标志位 11）
        if !name_bytes.is_ascii() && flags & 0x0800 == 0 {
            return Err(bad("名字不是 UTF-8".into()));
        }
        let name = std::str::from_utf8(name_bytes).map_err(|_| bad("名字不是 UTF-8".into()))?;
        if name == format!("{folder}/") {
            continue;
        }
        let file = name
            .strip_prefix(&folder)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|rest| PACKAGE_FILES.iter().find(|known| **known == rest))
            .ok_or_else(|| bad(format!("不认识的文件：{name}")))?;
        if flags & 0x0001 != 0 || size as u64 > MAX_ARCHIVE_BYTES {
            return Err(bad(format!("{name}：加密或过大")));
        }
        let local_name = usize::from(le16(zip, local + 26).ok_or_else(|| bad("本地头".into()))?);
        let local_extra = usize::from(le16(zip, local + 28).ok_or_else(|| bad("本地头".into()))?);
        // 本地头的名字必须与中央目录一致：两处名字不同是常见的混淆手法
        if le32(zip, local) != Some(0x0403_4b50)
            || zip.get(local + 30..local + 30 + local_name) != Some(name_bytes)
        {
            return Err(bad(format!("{name}：本地头")));
        }
        let start = local + 30 + local_name + local_extra;
        let data = zip
            .get(start..start + packed)
            .ok_or_else(|| bad(format!("{name}：数据越界")))?;
        let content = match method {
            0 => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, size)
                .map_err(|error| bad(format!("{name}：{error:?}")))?,
            other => return Err(bad(format!("{name}：压缩方式 {other}"))),
        };
        if content.len() != size || yisi_part::crc32::crc32(&[&content]) != crc {
            return Err(bad(format!("{name}：大小或 CRC 不对")));
        }
        if files.iter().any(|(known, _)| known == file) {
            return Err(bad(format!("{name}：重复")));
        }
        files.push((*file, content));
    }
    let missing: Vec<&str> = PACKAGE_FILES[..REQUIRED_FILES]
        .iter()
        .copied()
        .filter(|required| !files.iter().any(|(file, _)| file == required))
        .collect();
    if !missing.is_empty() {
        return Err(bad(format!("缺少 {}", missing.join("、"))));
    }
    Ok(files)
}

fn old_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(OLD_SUFFIX);
    PathBuf::from(name)
}

/// 程序包里的文件装到哪：主程序换成正在运行的程序文件，其余放在程序文件夹里同名。
fn destination(file: &str, program_dir: &Path, main_exe: &Path) -> PathBuf {
    if file == PACKAGE_MAIN_EXE {
        main_exe.to_path_buf()
    } else {
        program_dir.join(file)
    }
}

/// 把 `staged` 里的新文件换到各自的位置：原文件先改名为 `.old`（正在运行的程序也能改名），新文件再挪过去。
/// 任何一步失败都把已经换过的改回去，返回错误；程序文件夹保持原样。
/// 每换好一个文件调用一次 `replaced`（参数是已换好的个数）。
fn replace(
    staged: &[(PathBuf, PathBuf)],
    replaced: &mut dyn FnMut(usize),
) -> Result<(), HostError> {
    // 已经做完的步骤：(目标, 原文件有没有被改名为 .old)
    let mut done: Vec<(&Path, bool)> = Vec::new();
    let result = staged.iter().try_for_each(|(new, target)| {
        let old = old_path(target);
        if old.exists() {
            std::fs::remove_file(&old)?;
        }
        let had_old = target.exists();
        if had_old {
            std::fs::rename(target, &old)?;
        }
        done.push((target, had_old));
        std::fs::rename(new, target)?;
        replaced(done.len());
        Ok::<(), std::io::Error>(())
    });
    if let Err(error) = result {
        for (target, had_old) in done.into_iter().rev() {
            let _ = std::fs::remove_file(target);
            if had_old {
                let _ = std::fs::rename(old_path(target), target);
            }
        }
        return Err(HostError::new("updateReplace", error.to_string()));
    }
    Ok(())
}

/// 更新后第一次启动时调用：删掉换下来的 `.old` 与解压目录。删不掉的（例如被杀毒软件占着）留到下次。
pub fn clean_up() {
    let Ok(dir) = program_dir() else { return };
    if let Ok(main_exe) = std::env::current_exe() {
        for file in PACKAGE_FILES {
            let _ = std::fs::remove_file(old_path(&destination(file, &dir, &main_exe)));
        }
    }
    if let Ok(data) = data_dir() {
        let _ = std::fs::remove_dir_all(data.join("update"));
    }
}

pub fn check(current: &str) -> Result<UpdateInfo, HostError> {
    let ranked = rank(ask_all(Source::all_from_env()))?;
    let (source, release) = ranked
        .into_iter()
        .next()
        .ok_or_else(|| HostError::new("updateNoRelease", ""))?;
    Ok(UpdateInfo {
        current: current.to_owned(),
        available: compare_versions(&release.version, current) == Ordering::Greater,
        latest: release.version,
        source: source.name,
    })
}

/// 下载、核对、换上新版本，返回装上的版本号；之后由调用方重启。`busy` 为真（有任务在运行）时不更新。
/// 进度经 `report` 报告：先是下载（按字节，约每 1% 一次），再是安装的三步。
pub fn install(
    current: &str,
    busy: bool,
    report: &dyn Fn(UpdateProgress),
) -> Result<String, HostError> {
    if busy {
        return Err(HostError::new("updateBusy", ""));
    }
    if INSTALLING.swap(true, AtomicOrdering::SeqCst) {
        return Err(HostError::new("updateBusy", "正在更新"));
    }
    let result = install_inner(current, report);
    INSTALLING.store(false, AtomicOrdering::SeqCst);
    result
}

fn install_inner(current: &str, report: &dyn Fn(UpdateProgress)) -> Result<String, HostError> {
    let ranked = rank(ask_all(Source::all_from_env()))?;
    let mut first_error = None;
    for (index, (source, release)) in ranked.iter().enumerate() {
        let can_switch = index + 1 < ranked.len();
        match download(source, release, current, can_switch, report) {
            Ok((manifest, archive)) => return install_files(source, &manifest, &archive, report),
            Err(error) => {
                tracing::warn!(source = source.name, ?error, "从这个源更新没成，换下一个");
                // 「太慢换源」不如后面那个源的具体错误有用
                if first_error
                    .as_ref()
                    .is_none_or(|known: &HostError| known.code == "updateSlow")
                {
                    first_error = Some(error);
                }
            }
        }
    }
    Err(first_error.unwrap_or_else(|| HostError::new("updateNoRelease", "")))
}

/// 从一个源下载签名清单与压缩包，核对清单签名与压缩包的大小、摘要。`can_switch` 为真（还有别的源）时，
/// 下得太慢就中止，返回 `updateSlow`。
fn download(
    source: &Source,
    release: &Release,
    current: &str,
    can_switch: bool,
    report: &dyn Fn(UpdateProgress),
) -> Result<(Manifest, Vec<u8>), HostError> {
    let signature = release
        .signature
        .as_ref()
        .ok_or_else(|| HostError::new("updateUnsigned", release.version.clone()))?;
    let archive_asset = release
        .archive
        .as_ref()
        .ok_or_else(|| HostError::new("updateNoRelease", release.archive_name.clone()))?;
    let manifest = parse_manifest(&http_get(
        &signature.url,
        MAX_SIGNATURE_BYTES,
        false,
        &mut |_| true,
    )?)?;
    check_manifest(&manifest, release, current)?;

    // ① 下载：总量取签名清单里记的大小；约每 1% 报一次；太慢且有别的源时中止
    let total = manifest.size;
    report(UpdateProgress::download(source.name, 0, total));
    let started = Instant::now();
    let mut reported = 0u64;
    let mut slow = false;
    let archive = http_get(&archive_asset.url, total as usize, false, &mut |received| {
        let received = received as u64;
        if received >= total || received.saturating_sub(reported) * 100 >= total {
            reported = received;
            report(UpdateProgress::download(source.name, received, total));
        }
        slow = can_switch && too_slow(started.elapsed(), received, total);
        !slow
    });
    let archive = match archive {
        Err(_) if slow => return Err(HostError::new("updateSlow", source.name)),
        other => other?,
    };
    check_archive(&archive, &manifest, archive_asset)?;
    Ok((manifest, archive))
}

/// ② 安装：核对（签名在下载时已核对，这里核对程序包的内容）→ 解压 → 换上。
fn install_files(
    source: &Source,
    manifest: &Manifest,
    archive: &[u8],
    report: &dyn Fn(UpdateProgress),
) -> Result<String, HostError> {
    let name = source.name;
    report(UpdateProgress::install(name, "verify", 0, 1));
    let files = extract(archive, &manifest.version)?;
    report(UpdateProgress::install(name, "verify", 1, 1));

    let program =
        program_dir().map_err(|error| HostError::new("updateReplace", error.to_string()))?;
    let main_exe = std::env::current_exe()
        .map_err(|error| HostError::new("updateReplace", error.to_string()))?;
    let staging = data_dir()
        .map(|data| data.join("update").join(format!("v{}", manifest.version)))
        .map_err(|error| HostError::new("updateReplace", error.to_string()))?;
    let count = files.len();
    report(UpdateProgress::install(name, "extract", 0, count));
    let write_staged = || -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        files
            .iter()
            .enumerate()
            .map(|(index, (file, content))| {
                let staged = staging.join(file);
                std::fs::write(&staged, content)?;
                report(UpdateProgress::install(name, "extract", index + 1, count));
                Ok((staged, destination(file, &program, &main_exe)))
            })
            .collect()
    };
    let staged =
        write_staged().map_err(|error| HostError::new("updateReplace", error.to_string()))?;
    report(UpdateProgress::install(name, "replace", 0, count));
    replace(&staged, &mut |done| {
        report(UpdateProgress::install(name, "replace", done, count));
    })?;
    Ok(manifest.version.clone())
}

/// 启动更新后的程序并退出当前进程。新进程带上当前 PID，等当前进程退出后才建窗口。
pub fn restart(app: &tauri::AppHandle) -> Result<(), HostError> {
    let exe = std::env::current_exe()
        .map_err(|error| HostError::new("updateRestart", error.to_string()))?;
    std::process::Command::new(exe)
        .arg(AFTER_UPDATE_ARG)
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|error| HostError::new("updateRestart", error.to_string()))?;
    app.exit(0);
    Ok(())
}

/// 启动参数里有 `--after-update <pid>` 时，等那个进程退出（最多 30 秒），再清理换下来的旧文件。
pub fn after_update(args: &[String]) {
    let Some(at) = args.iter().position(|arg| arg == AFTER_UPDATE_ARG) else {
        return;
    };
    if let Some(pid) = args.get(at + 1).and_then(|pid| pid.parse::<u32>().ok()) {
        wait_for_exit(pid);
    }
    clean_up();
}

fn wait_for_exit(pid: u32) {
    #[cfg(windows)]
    yisi_io::wait_for_process_exit(pid, std::time::Duration::from_secs(30));
    #[cfg(not(windows))]
    let _ = pid;
}

/// 核对清单签名（ECDSA P-256 / SHA-256，系统的 CNG 来验）。
fn verify_signature(
    message: &[u8],
    signature: &[u8; 64],
    public_key: &[u8; 64],
) -> Result<bool, HostError> {
    #[cfg(windows)]
    {
        let hash: [u8; 32] = Sha256::digest(message).into();
        yisi_io::verify_p256(&hash, signature, public_key)
            .map_err(|error| HostError::new("updateSignature", error.to_string()))
    }
    #[cfg(not(windows))]
    {
        let _ = (message, signature, public_key);
        Err(HostError::new("unsupportedPlatform", ""))
    }
}

/// GET 一个地址（WinHTTP），`api` 为真时带上 GitHub API 要的 Accept 头；`received` 见 `yisi_io::http_get`。
fn http_get(
    url: &str,
    limit: usize,
    api: bool,
    received: &mut dyn FnMut(usize) -> bool,
) -> Result<Vec<u8>, HostError> {
    #[cfg(windows)]
    {
        use yisi_io::HttpError;
        let headers: &[&str] = if api {
            &["Accept: application/vnd.github+json"]
        } else {
            &[]
        };
        let agent = format!("yisi-disk-tools/{}", env!("CARGO_PKG_VERSION"));
        yisi_io::http_get(url, limit, headers, &agent, received).map_err(|error| match error {
            HttpError::Status(status) => HostError::new("updateHttp", format!("{status} {url}")),
            HttpError::TooLarge(_) => HostError::new("updateMismatch", format!("{error} {url}")),
            other => HostError::new("updateNetwork", other.to_string()),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (url, limit, api, received);
        Err(HostError::new("unsupportedPlatform", ""))
    }
}

#[cfg(test)]
mod tests;
