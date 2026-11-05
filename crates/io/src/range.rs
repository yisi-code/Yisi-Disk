//! 字节区间。

/// 两段（起点、长度）是否重叠：一段的起点在另一段的末尾之前（相接不算）；末端溢出时按到头算。
pub fn ranges_overlap(a_offset: u64, a_length: u64, b_offset: u64, b_length: u64) -> bool {
    a_offset < b_offset.saturating_add(b_length) && b_offset < a_offset.saturating_add(a_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touching_ranges_do_not_overlap() {
        assert!(ranges_overlap(0, 10, 9, 1));
        assert!(!ranges_overlap(0, 10, 10, 5));
        // 长度为 0 的点落在另一段里面也算
        assert!(ranges_overlap(5, 0, 0, 10));
        assert!(!ranges_overlap(10, 0, 0, 10));
        assert!(ranges_overlap(u64::MAX - 1, 10, u64::MAX - 1, 1));
    }
}
