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

    /// 签名命中后，用签名所在的一块数据进一步确认；确认是这个格式时返回恢复状态。
    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>>;

    /// 签名所在的那一块离文件开头多远（字节）。绝大多数格式的签名就在文件开头，是 0；
    /// ISO 9660 的卷描述符从 32 KiB 处开始，前面的系统区通常全是 0、认不出来，就在卷描述符那一块认，
    /// 文件从它前面 32 KiB 算起（深度恢复回到那里，从文件开头起把数据交给 [`Recovering`]）。
    fn lead(&self) -> u64 {
        0
    }
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
    /// 用格式本身的扩展名。文件完整（[`DataCheck::Complete`] 或 [`finish`](Self::finish) 给出长度）之后调用；
    /// 部分恢复时在 [`intact`](Self::intact) 之后也调用，给出到断点为止能确定的说法。
    fn extension(&self) -> Option<&'static str> {
        None
    }

    /// 部分恢复：文件没能完整（数据在设备末尾或读不出的块处断了、范围里出现了别的文件头、结构走到一半对不上了、
    /// 要的数据超出设备末尾），从文件开头起还能确定属于它的那一段的长度，不超过 `received`
    /// （已经交给 [`data_check`](Self::data_check) 的字节数）。
    ///
    /// 只在至少核对过一个完整单元（一个帧、一页、一个成员、一张图像，或文件头之后的真正内容）时给出，
    /// 不然随机数据里的签名也会变成「不完整的文件」；`None` 表示说不出可靠的一段，不保存（默认）。
    /// 调用之后不再送数据。
    fn intact(&mut self, _received: u64) -> Option<u64> {
        None
    }
}
