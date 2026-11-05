//! 格式插件的接口。

/// 格式的基本信息，界面上的格式列表来自这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatInfo {
    /// 扩展名，同时作为格式的标识，不带点。
    pub extension: &'static str,
    /// 按内容可能改用的扩展名（[`Recovering::extension`]），例如 ZIP 里装的是 Word 文档时存成 docx。
    /// 注册表按它们也能找回这个格式（另存时按扫描时的扩展名重新识别）。
    pub variants: &'static [&'static str],
    /// 英文简述；界面按扩展名取本地化文案，取不到时显示它。
    pub description: &'static str,
    /// 超过这个大小仍未结束的候选文件视为误识别。
    pub max_size: u64,
    pub enabled_by_default: bool,
    /// 里面可能装着别的格式的完整文件、而且可能正好落在块边界上（压缩包、复合文档里的图片、TIFF 与 RAW 的 JPEG 预览、
    /// 数据库里存的图片）。为 `false` 的格式（音视频、单张图片）在按文件头声明的长度取数据时，
    /// 中途某个块开头认出了别的文件头，说明这个文件后面已经被别的文件覆盖：放弃它，从那个块重新找。
    pub nests_files: bool,
}

/// 文件头签名：文件开头第 `offset` 字节起是 `bytes`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature {
    pub offset: u16,
    pub bytes: &'static [u8],
}

/// 送入一块数据后的判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataCheck {
    /// 还没结束，继续送数据。
    Continue,
    /// 文件在这个长度处结束；长度可能超过已送入的数据，此时按长度继续取数据，不再检查。
    Complete(u64),
    /// 数据与格式不符，这不是一个有效文件。
    Invalid,
}

pub trait FileFormat: Send + Sync {
    fn info(&self) -> &FormatInfo;

    fn signatures(&self) -> &[Signature];

    /// 签名命中后，用文件开头的一块数据进一步确认；确认是这个格式时返回恢复状态。
    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>>;
}

/// 一个正在恢复的文件。数据从文件开头起按顺序分块送入，块与块首尾相接。
pub trait Recovering: Send {
    /// `offset` 是这一块在文件里的偏移。
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck;

    /// 没有自身结束标志的格式（例如 PDF）返回 true：遇到下一个文件头时停下，由 [`finish`](Self::finish) 定长度。
    fn open_ended(&self) -> bool {
        false
    }

    /// 数据到此为止（遇到下一个文件头、达到最大尺寸或设备结束），`received` 是已送入的字节数。
    /// 返回文件长度；`None` 表示不是完整文件，丢弃。
    fn finish(&mut self, received: u64) -> Option<u64>;

    /// 正在等待的数据位置（相对文件开头）。调用方据此尽早排除长度离谱的误识别：
    /// 位置已超出设备末尾或格式的最大尺寸时，不必再读下去。
    fn waiting_at(&self) -> Option<u64> {
        None
    }

    /// 按内容定的扩展名，是格式的 [`FormatInfo::variants`] 之一；没有更具体的说法时为 `None`，
    /// 用格式本身的扩展名。文件完整（[`DataCheck::Complete`] 或 [`finish`](Self::finish) 给出长度）之后调用。
    fn extension(&self) -> Option<&'static str> {
        None
    }
}
