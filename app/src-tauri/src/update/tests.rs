use super::*;

/// 用发版私钥签过的一份清单：版本 9.9.9，压缩包内容是 `abc`。
const SIGNED_BODY: &str = "yisi-disk-tools update v1\nversion: 9.9.9\nfile: yisi-disk-tools-v9.9.9-windows-x64.zip\nsize: 3\nsha256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n";
const SIGNED_SIGNATURE: &str = "9b6bd5113a2baaea1a5d0a60bc8ca0c4ebc04e2abb34bf92fcffccd0c5bd869485a0bbec19292e16c71114846942c47bfef9b4f78ac338458aba1191fe221317";

fn signed_manifest() -> String {
    format!("{SIGNED_BODY}signature: {SIGNED_SIGNATURE}\n")
}

fn github() -> Source {
    Source::github()
}

fn release_json(tag: &str, with_signature: bool) -> Vec<u8> {
    let name = archive_name(tag.trim_start_matches('v'));
    let mut assets = vec![serde_json::json!({
        "name": name,
        "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/{name}"),
        "size": 3,
        "digest": "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
    })];
    if with_signature {
        assets.push(serde_json::json!({
            "name": format!("{name}.sig"),
            "browser_download_url": format!("{DOWNLOAD_PREFIX}{tag}/{name}.sig"),
            "size": 300,
        }));
    }
    serde_json::to_vec(&serde_json::json!({ "tag_name": tag, "assets": assets })).unwrap()
}

fn answer(source: Source, tag: Option<&str>, millis: u64) -> Answer {
    Answer {
        release: match tag {
            Some(tag) => parse_release(&release_json(tag, true), &github()),
            None => Err(HostError::new("updateNetwork", "连不上")),
        },
        source,
        elapsed: Duration::from_millis(millis),
    }
}

#[test]
fn the_newest_version_wins_and_sources_are_ordered_by_speed() {
    // 两边版本相同：快的在前
    let ranked = rank(vec![
        answer(Source::github(), Some("v9.9.9"), 900),
        answer(Source::gitee(), Some("v9.9.9"), 120),
    ])
    .unwrap();
    let names: Vec<&str> = ranked.iter().map(|(source, _)| source.name).collect();
    assert_eq!(names, ["gitee", "github"]);

    // 镜像还没同步到最新版：只用有最新版的源，哪怕它慢
    let ranked = rank(vec![
        answer(Source::github(), Some("v9.9.10"), 900),
        answer(Source::gitee(), Some("v9.9.9"), 120),
    ])
    .unwrap();
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].0.name, "github");
    assert_eq!(ranked[0].1.version, "9.9.10");

    // 一个源连不上不影响另一个
    let ranked = rank(vec![
        answer(Source::github(), None, 15_000),
        answer(Source::gitee(), Some("v9.9.9"), 300),
    ])
    .unwrap();
    assert_eq!(ranked[0].0.name, "gitee");

    // 都连不上：两边的原因都报出来
    let error = rank(vec![
        answer(Source::github(), None, 15_000),
        answer(Source::gitee(), None, 15_000),
    ])
    .unwrap_err();
    assert_eq!(error.code, "updateNetwork");
    assert!(error.detail.contains("github") && error.detail.contains("gitee"));
}

#[test]
fn local_sources_for_walkthroughs_accept_only_this_computer() {
    let sources = Source::local("http://127.0.0.1:8766/latest, http://127.0.0.1:8767/latest");
    let names: Vec<&str> = sources.iter().map(|source| source.name).collect();
    assert_eq!(names, ["local", "local-2"]);
    assert_eq!(sources[1].asset_prefix, "http://127.0.0.1:8767/");
    assert!(Source::local("https://evil.example/latest").is_empty());
    // 只是前缀相像的地址也不认
    assert!(Source::local("http://127.0.0.10:80/latest").is_empty());
}

#[test]
fn a_download_is_too_slow_when_the_rest_would_take_over_a_minute() {
    let mib = 1 << 20;
    // 刚开始不判
    assert!(!too_slow(Duration::from_secs(3), 10_000, 6 * mib));
    // 8 秒下了 1 MiB，剩 5 MiB 还要 40 秒：不换
    assert!(!too_slow(Duration::from_secs(8), mib, 6 * mib));
    // 8 秒只下了 512 KiB，剩下的要 88 秒：换
    assert!(too_slow(Duration::from_secs(8), mib / 2, 6 * mib));
    // 15 秒一个字节都没收到：换
    assert!(!too_slow(Duration::from_secs(10), 0, 6 * mib));
    assert!(too_slow(Duration::from_secs(15), 0, 6 * mib));
    // 已经下完：不换
    assert!(!too_slow(Duration::from_secs(120), 6 * mib, 6 * mib));
}

#[test]
fn gitee_assets_and_pages_are_accepted_only_under_the_mirror() {
    let name = archive_name("9.9.9");
    let json = serde_json::to_vec(&serde_json::json!({
        "tag_name": "v9.9.9",
        "assets": [
            { "name": name, "browser_download_url": format!("{GITEE_DOWNLOAD_PREFIX}v9.9.9/{name}") },
            { "name": format!("{name}.sig"), "browser_download_url": format!("https://example.com/{name}.sig") },
        ],
    }))
    .unwrap();
    let release = parse_release(&json, &Source::gitee()).unwrap();
    assert!(release.archive.is_some());
    // 签名清单不在镜像的地址下：不认
    assert!(release.signature.is_none());
    assert_eq!(releases_page(Some("gitee")), GITEE_PAGE);
    assert_eq!(releases_page(Some("github")), GITHUB_PAGE);
    assert_eq!(releases_page(Some("别的")), GITHUB_PAGE);
    assert_eq!(releases_page(None), GITHUB_PAGE);
}

#[test]
fn versions_compare_by_number_segments() {
    assert_eq!(compare_versions("0.10.0", "0.9.9"), Ordering::Greater);
    assert_eq!(compare_versions("v0.2.0", "0.2"), Ordering::Equal);
    assert_eq!(compare_versions("0.2.1-beta", "0.2.1"), Ordering::Equal);
    assert_eq!(compare_versions("0.2.0", "0.3.0"), Ordering::Less);
}

#[test]
fn release_assets_are_found_only_under_the_allowed_prefix() {
    let release = parse_release(&release_json("v9.9.9", true), &github()).unwrap();
    assert_eq!(release.version, "9.9.9");
    let archive = release.archive.unwrap();
    assert_eq!(archive.size, 3);
    assert_eq!(
        archive.sha256.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert!(release.signature.is_some());

    // 本机假源下，GitHub 的地址不认
    let local = Source {
        name: "local",
        api: "http://127.0.0.1:8123/latest".into(),
        asset_prefix: "http://127.0.0.1:8123/".into(),
    };
    let release = parse_release(&release_json("v9.9.9", true), &local).unwrap();
    assert!(release.archive.is_none() && release.signature.is_none());

    // 版本号里不许有路径字符
    assert!(parse_release(br#"{"tag_name":"v1/../x","assets":[]}"#, &github()).is_err());
}

#[test]
fn a_signed_manifest_verifies_and_any_change_breaks_it() {
    let manifest = parse_manifest(signed_manifest().as_bytes()).unwrap();
    assert_eq!(manifest.version, "9.9.9");
    assert_eq!(manifest.size, 3);
    assert!(verify_signature(&manifest.signed, &manifest.signature, &PUBLIC_KEY).unwrap());

    let release = parse_release(&release_json("v9.9.9", true), &github()).unwrap();
    check_manifest(&manifest, &release, "0.2.0").unwrap();
    check_archive(b"abc", &manifest, release.archive.as_ref().unwrap()).unwrap();
    assert_eq!(
        check_archive(b"abd", &manifest, release.archive.as_ref().unwrap())
            .unwrap_err()
            .code,
        "updateMismatch"
    );

    // 改清单里任何一个字节，签名都不对
    let forged = signed_manifest().replace("size: 3", "size: 4");
    let forged = parse_manifest(forged.as_bytes()).unwrap();
    assert_eq!(
        check_manifest(&forged, &release, "0.2.0").unwrap_err().code,
        "updateSignature"
    );
}

#[test]
fn an_old_signed_version_is_not_installed_again() {
    let manifest = parse_manifest(signed_manifest().as_bytes()).unwrap();
    let release = parse_release(&release_json("v9.9.9", true), &github()).unwrap();
    assert_eq!(
        check_manifest(&manifest, &release, "9.9.9")
            .unwrap_err()
            .code,
        "updateNotNewer"
    );
    // GitHub 说最新是别的版本，签名清单却是 9.9.9：不装
    let other = parse_release(&release_json("v9.9.10", true), &github()).unwrap();
    assert_eq!(
        check_manifest(&manifest, &other, "0.2.0").unwrap_err().code,
        "updateMismatch"
    );
}

#[test]
fn malformed_manifests_are_rejected() {
    for text in [
        "".to_owned(),
        signed_manifest().replace(MANIFEST_HEADER, "other"),
        signed_manifest().replace("signature: ", "signature: 00"),
        signed_manifest().replace("version: 9.9.9\n", ""),
        format!("{SIGNED_BODY}extra: 1\nsignature: {SIGNED_SIGNATURE}\n"),
    ] {
        assert!(parse_manifest(text.as_bytes()).is_err(), "{text}");
    }
}

/// 拼一个 zip：每项 (名字, 内容, 是否压缩)。名字含非 ASCII 时置 UTF-8 标志。
fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, content, deflate) in entries {
        let data = if *deflate {
            miniz_oxide::deflate::compress_to_vec(content, 6)
        } else {
            content.to_vec()
        };
        let crc = yisi_part::crc32::crc32(&[content]);
        let flags: u16 = if name.is_ascii() { 0 } else { 0x0800 };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0]);
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0]);
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(content.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn package(extra: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let folder = package_folder("9.9.9");
    let names: Vec<String> = [
        "",
        PACKAGE_MAIN_EXE,
        "yisi-engine.exe",
        "yisi-broker.exe",
        "LICENSE.md",
    ]
    .iter()
    .map(|file| format!("{folder}/{file}"))
    .collect();
    let mut entries: Vec<(&str, &[u8], bool)> = vec![
        (&names[0], b"", false),
        (&names[1], b"main program", true),
        (&names[2], b"engine", true),
        (&names[3], b"broker", false),
        (&names[4], b"license", true),
    ];
    entries.extend_from_slice(extra);
    zip(&entries)
}

#[test]
fn the_package_is_extracted_with_only_known_files() {
    let files = extract(&package(&[]), "9.9.9").unwrap();
    let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        [
            PACKAGE_MAIN_EXE,
            "yisi-engine.exe",
            "yisi-broker.exe",
            "LICENSE.md"
        ]
    );
    assert_eq!(files[0].1, b"main program");
}

#[test]
fn unexpected_or_damaged_packages_are_rejected() {
    let folder = package_folder("9.9.9");
    let traversal = format!("{folder}/../evil.exe");
    let unknown = format!("{folder}/evil.dll");
    let nested = format!("{folder}/data/x.json");
    for extra in [
        &traversal,
        &unknown,
        &nested,
        &"other/yisi-engine.exe".to_owned(),
    ] {
        let error = extract(&package(&[(extra, b"x", false)]), "9.9.9").unwrap_err();
        assert_eq!(error.code, "updateArchive", "{extra}");
    }
    // 版本对不上（顶层文件夹名不同）
    assert!(extract(&package(&[]), "9.9.8").is_err());
    // 缺主程序
    let only_license = zip(&[(&format!("{folder}/LICENSE.md"), b"x", false)]);
    assert!(extract(&only_license, "9.9.9").is_err());
    // 内容被改（CRC 不对）：代理的内容没压缩，紧跟在本地头的名字后面
    let mut damaged = package(&[]);
    let at = damaged
        .windows(10)
        .position(|window| window == b".exebroker")
        .unwrap();
    damaged[at + 4] = b'B';
    let error = extract(&damaged, "9.9.9").unwrap_err();
    assert!(error.detail.contains("CRC"), "{}", error.detail);
    // 本地头的名字与中央目录不同
    let mut renamed = package(&[]);
    let at = renamed
        .windows(10)
        .position(|window| window == b"yisi-broke")
        .unwrap();
    renamed[at] = b'Y';
    let error = extract(&renamed, "9.9.9").unwrap_err();
    assert!(error.detail.contains("本地头"), "{}", error.detail);
    // 不是 zip
    assert!(extract(b"not a zip", "9.9.9").is_err());
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yisi-update-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn files_are_swapped_and_old_ones_kept_aside() {
    let dir = temp_dir("swap");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(dir.join("app.exe"), b"old app").unwrap();
    std::fs::write(staging.join("app.exe"), b"new app").unwrap();
    std::fs::write(staging.join("LICENSE.md"), b"new license").unwrap();
    let mut reported = Vec::new();
    replace(
        &[
            (staging.join("app.exe"), dir.join("app.exe")),
            (staging.join("LICENSE.md"), dir.join("LICENSE.md")),
        ],
        &mut |done| reported.push(done),
    )
    .unwrap();
    assert_eq!(reported, [1, 2]);
    assert_eq!(std::fs::read(dir.join("app.exe")).unwrap(), b"new app");
    assert_eq!(std::fs::read(dir.join("app.exe.old")).unwrap(), b"old app");
    assert_eq!(
        std::fs::read(dir.join("LICENSE.md")).unwrap(),
        b"new license"
    );
    assert!(!dir.join("LICENSE.md.old").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_failed_swap_puts_everything_back() {
    let dir = temp_dir("rollback");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(dir.join("app.exe"), b"old app").unwrap();
    std::fs::write(dir.join("engine.exe"), b"old engine").unwrap();
    std::fs::write(staging.join("app.exe"), b"new app").unwrap();
    // 第二个新文件不存在：换到这一步失败
    let mut reported = Vec::new();
    let error = replace(
        &[
            (staging.join("app.exe"), dir.join("app.exe")),
            (staging.join("engine.exe"), dir.join("engine.exe")),
        ],
        &mut |done| reported.push(done),
    )
    .unwrap_err();
    assert_eq!(reported, [1]);
    assert_eq!(error.code, "updateReplace");
    assert_eq!(std::fs::read(dir.join("app.exe")).unwrap(), b"old app");
    assert_eq!(
        std::fs::read(dir.join("engine.exe")).unwrap(),
        b"old engine"
    );
    assert!(!dir.join("app.exe.old").exists() && !dir.join("engine.exe.old").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
