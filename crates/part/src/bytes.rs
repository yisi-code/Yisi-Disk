//! 从字节切片里按偏移取整数：实现在 `yisi_io::bytes`（E01 镜像的读取也要用，`yisi-io` 在依赖的最底层），
//! 这里原样转出，各 crate 照旧用 `yisi_part::bytes`。

pub use yisi_io::bytes::*;
