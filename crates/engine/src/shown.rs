//! 分区里的一项在各种显示条件下列不列：目录树的显示与另存时复制哪些项共用这一条规则。
//!
//! 显示条件是三个勾选（[`ShowFilter`]：未删除、已删除、隐藏与系统文件），共 8 种组合，编号见
//! [`ShowFilter::index`]。一项在哪几种组合下列出记成 8 位的掩码（第 n 位对应编号 n），界面按当前勾选取那一位：
//!
//! - 文件：自身状态被勾选（已删除的看「已删除」，未删除的看「未删除」）；带隐藏或系统属性的还要勾「隐藏」。
//! - 文件夹：带隐藏或系统属性的要勾「隐藏」；并且里面至少有一项列出，或者它本来就是空的、自身状态被勾选。
//!   里面一项都不符合的文件夹不列，免得用户点开只看到「没有符合条件的项」。
//! - 文件夹里面的情况没算出来（太大，限定时间内没走完）时按 [`unknown_dir_mask`]：按自身列，未删除的文件夹
//!   在勾了「已删除」时也列（删除的项可能在里面）。

use yisi_engine::protocol::ShowFilter;

/// 8 种组合里满足 `allowed` 的那几位。
fn mask_where(allowed: impl Fn(ShowFilter) -> bool) -> u8 {
    (0u8..8)
        .filter(|&index| allowed(ShowFilter::from_index(index)))
        .fold(0, |mask, index| mask | 1 << index)
}

/// 有项能列出的组合：「未删除」「已删除」至少勾了一个（都不勾时什么也不列）。一个文件夹的掩码最多到这些位。
pub const REACHABLE: u8 = 0b1110_1110;

/// 隐藏属性这一关：带隐藏或系统属性（`hidden`）的只在勾了「隐藏」时过。
fn hidden_ok(hidden: bool) -> u8 {
    mask_where(|filter| !hidden || filter.hidden)
}

/// 一项自身（不看里面）在哪些组合下列出。`hidden` 是带隐藏或系统属性。
pub fn own_mask(deleted: bool, hidden: bool) -> u8 {
    hidden_ok(hidden) & mask_where(|filter| if deleted { filter.deleted } else { filter.live })
}

/// 文件夹在哪些组合下列出：`children` 是里面各项掩码的或，`empty` 是里面本来就一项也没有。
pub fn dir_mask(deleted: bool, hidden: bool, children: u8, empty: bool) -> u8 {
    let own_when_empty = if empty { own_mask(deleted, hidden) } else { 0 };
    hidden_ok(hidden) & (children | own_when_empty)
}

/// 里面的情况没算出来的文件夹：按自身列；未删除的文件夹勾了「已删除」时也列。
pub fn unknown_dir_mask(deleted: bool, hidden: bool) -> u8 {
    let as_path = if deleted {
        0
    } else {
        hidden_ok(hidden) & mask_where(|filter| filter.deleted)
    };
    own_mask(deleted, hidden) | as_path
}

/// 这个掩码在 `filter` 下列不列。
pub fn shown_by(mask: u8, filter: ShowFilter) -> bool {
    mask & 1 << filter.index() != 0
}

/// 进入这个文件夹之前就能定下来不列的：带隐藏或系统属性、又没勾「隐藏」（里面的项再符合也不列）。
pub fn blocked_by(hidden: bool, filter: ShowFilter) -> bool {
    !shown_by(hidden_ok(hidden), filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: ShowFilter = ShowFilter {
        live: true,
        deleted: false,
        hidden: false,
    };
    const DELETED: ShowFilter = ShowFilter {
        live: false,
        deleted: true,
        hidden: false,
    };
    const LIVE_HIDDEN: ShowFilter = ShowFilter {
        live: true,
        deleted: false,
        hidden: true,
    };
    const ALL: ShowFilter = ShowFilter {
        live: true,
        deleted: true,
        hidden: true,
    };

    #[test]
    fn reachable_is_where_live_or_deleted_is_ticked() {
        assert_eq!(
            REACHABLE,
            mask_where(|filter| filter.live || filter.deleted)
        );
    }

    #[test]
    fn indexes_round_trip() {
        for index in 0..8 {
            assert_eq!(ShowFilter::from_index(index).index(), index);
        }
    }

    #[test]
    fn files_follow_their_own_state_and_attributes() {
        let live = own_mask(false, false);
        assert!(shown_by(live, LIVE) && !shown_by(live, DELETED));
        let deleted = own_mask(true, false);
        assert!(!shown_by(deleted, LIVE) && shown_by(deleted, DELETED));
        let hidden = own_mask(false, true);
        assert!(!shown_by(hidden, LIVE) && shown_by(hidden, LIVE_HIDDEN));
    }

    #[test]
    fn a_folder_is_listed_only_when_something_inside_is() {
        // 未删除的文件夹里只有已删除的文件：只勾「未删除」时不列，勾了「已删除」时列
        let inside = own_mask(true, false);
        let folder = dir_mask(false, false, inside, false);
        assert!(!shown_by(folder, LIVE));
        assert!(shown_by(folder, DELETED));
        // 本来就空的文件夹按自身列
        let empty = dir_mask(false, false, 0, true);
        assert!(shown_by(empty, LIVE) && !shown_by(empty, DELETED));
        // 有内容、但都不符合：不当作空文件夹
        let filtered = dir_mask(false, false, 0, false);
        assert_eq!(filtered, 0);
        // 隐藏的文件夹里面再有符合的，没勾「隐藏」也不列
        let recycle = dir_mask(false, true, own_mask(false, false), false);
        assert!(!shown_by(recycle, LIVE) && shown_by(recycle, LIVE_HIDDEN));
        assert!(blocked_by(true, LIVE) && !blocked_by(true, ALL));
    }

    #[test]
    fn unknown_folders_keep_the_old_rule() {
        let live = unknown_dir_mask(false, false);
        assert!(shown_by(live, LIVE) && shown_by(live, DELETED));
        let deleted = unknown_dir_mask(true, false);
        assert!(!shown_by(deleted, LIVE) && shown_by(deleted, DELETED));
        let hidden = unknown_dir_mask(false, true);
        assert!(!shown_by(hidden, DELETED));
    }
}
