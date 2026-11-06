//! 核对 ECDSA P-256 签名，用 Windows 自带的 CNG。目前只有程序内更新在用（核对发布清单的签名）。

/// 用公钥 `public_key`（P-256 曲线上的点，X、Y 各 32 字节，大端）核对 `hash`（SHA-256 摘要）上的签名
/// `signature`（r、s 各 32 字节、大端拼接）。签名对得上为 `true`；CNG 本身出错（打不开算法、公钥导入失败）时
/// 返回错误。
#[cfg(windows)]
pub fn verify_p256(
    hash: &[u8; 32],
    signature: &[u8; 64],
    public_key: &[u8; 64],
) -> std::io::Result<bool> {
    use windows_sys::Win32::Security::Cryptography::{
        BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALGORITHM, BCRYPT_ECDSA_PUBLIC_P256_MAGIC,
        BCryptCloseAlgorithmProvider, BCryptDestroyKey, BCryptImportKeyPair,
        BCryptOpenAlgorithmProvider, BCryptVerifySignature,
    };

    // BCRYPT_ECCKEY_BLOB：魔数、坐标字节数，后面跟 X、Y
    let mut blob = Vec::with_capacity(8 + public_key.len());
    blob.extend_from_slice(&BCRYPT_ECDSA_PUBLIC_P256_MAGIC.to_le_bytes());
    blob.extend_from_slice(&32u32.to_le_bytes());
    blob.extend_from_slice(public_key);
    let failed = |step: &str, status: i32| {
        std::io::Error::other(format!("{step} 0x{:08x}", status.cast_unsigned()))
    };

    let mut algorithm = std::ptr::null_mut();
    // SAFETY: 输出参数指向本地变量；算法名是系统常量。成功后在本函数末尾关闭。
    let status = unsafe {
        BCryptOpenAlgorithmProvider(
            &mut algorithm,
            BCRYPT_ECDSA_P256_ALGORITHM,
            std::ptr::null(),
            0,
        )
    };
    if status != 0 {
        return Err(failed("BCryptOpenAlgorithmProvider", status));
    }
    let mut key = std::ptr::null_mut();
    // SAFETY: algorithm 是上面打开的句柄；blob 在调用期间有效，长度如实给出；key 是本地输出参数。
    let status = unsafe {
        BCryptImportKeyPair(
            algorithm,
            std::ptr::null_mut(),
            BCRYPT_ECCPUBLIC_BLOB,
            &mut key,
            blob.as_ptr(),
            blob.len() as u32,
            0,
        )
    };
    let result = if status == 0 {
        // SAFETY: key 是刚导入的公钥句柄；hash 与 signature 在调用期间有效，长度如实给出；没有填充参数。
        let verified = unsafe {
            BCryptVerifySignature(
                key,
                std::ptr::null(),
                hash.as_ptr(),
                hash.len() as u32,
                signature.as_ptr(),
                signature.len() as u32,
                0,
            )
        };
        // SAFETY: key 是上面导入成功的句柄，之后不再使用。
        unsafe { BCryptDestroyKey(key) };
        Ok(verified == 0)
    } else {
        Err(failed("BCryptImportKeyPair", status))
    };
    // SAFETY: algorithm 是上面打开成功的句柄，之后不再使用。
    unsafe { BCryptCloseAlgorithmProvider(algorithm, 0) };
    result
}

#[cfg(not(windows))]
pub fn verify_p256(
    _hash: &[u8; 32],
    _signature: &[u8; 64],
    _public_key: &[u8; 64],
) -> std::io::Result<bool> {
    Err(std::io::ErrorKind::Unsupported.into())
}
