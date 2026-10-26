//! 按分区类型（MBR 类型字节、GPT 类型 GUID）判断分区的用途，只覆盖常见的几种。

use crate::{Guid, Partition, PartitionKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    EfiSystem,
    MicrosoftReserved,
    BasicData,
    WindowsRecovery,
    /// Windows 动态磁盘（LDM）的元数据与数据分区。
    LdmMetadata,
    LdmData,
    StorageSpaces,
    Linux,
    LinuxSwap,
    LinuxLvm,
    Apfs,
    HfsPlus,
    Fat12,
    Fat16,
    Fat32,
    /// MBR 类型 0x07：NTFS 或 exFAT。
    NtfsOrExfat,
    /// MBR 里隐藏的 FAT / NTFS（类型字节加了 0x10）。
    Hidden,
    /// GPT 的保护性 MBR 项（主备 GPT 都读不出时才会列出）。
    GptProtective,
    /// 无分区表时的整块设备。
    Whole,
}

const EFI_SYSTEM: Guid = Guid::from_text("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
const MICROSOFT_RESERVED: Guid = Guid::from_text("E3C9E316-0B5C-4DB8-817D-F92DF00215AE");
const BASIC_DATA: Guid = Guid::from_text("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
const WINDOWS_RECOVERY: Guid = Guid::from_text("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC");
const LDM_METADATA: Guid = Guid::from_text("5808C8AA-7E8F-42E0-85D2-E1E90434CFB3");
const LDM_DATA: Guid = Guid::from_text("AF9B60A0-1431-4F62-BC68-3311714A69AD");
const STORAGE_SPACES: Guid = Guid::from_text("E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D");
const LINUX: Guid = Guid::from_text("0FC63DAF-8483-4772-8E79-3D69D8477DE4");
const LINUX_SWAP: Guid = Guid::from_text("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F");
const LINUX_LVM: Guid = Guid::from_text("E6D6D379-F507-44C2-A23C-238F2A3DF928");
const APFS: Guid = Guid::from_text("7C3457EF-0000-11AA-AA11-00306543ECAC");
const HFS_PLUS: Guid = Guid::from_text("48465300-0000-11AA-AA11-00306543ECAC");

impl Partition {
    /// 分区的用途；类型不在常见之列时为 `None`。
    pub fn role(&self) -> Option<Role> {
        match &self.kind {
            PartitionKind::Whole => Some(Role::Whole),
            PartitionKind::Mbr { type_id, .. } => match type_id {
                0x01 => Some(Role::Fat12),
                0x04 | 0x06 | 0x0E => Some(Role::Fat16),
                0x0B | 0x0C => Some(Role::Fat32),
                0x07 => Some(Role::NtfsOrExfat),
                0x11 | 0x14 | 0x16 | 0x17 | 0x1B | 0x1C | 0x1E => Some(Role::Hidden),
                0x27 => Some(Role::WindowsRecovery),
                0x42 => Some(Role::LdmData),
                0x82 => Some(Role::LinuxSwap),
                0x83 => Some(Role::Linux),
                0x8E => Some(Role::LinuxLvm),
                0xAF => Some(Role::HfsPlus),
                0xEE => Some(Role::GptProtective),
                0xEF => Some(Role::EfiSystem),
                _ => None,
            },
            PartitionKind::Gpt { type_guid, .. } => [
                (EFI_SYSTEM, Role::EfiSystem),
                (MICROSOFT_RESERVED, Role::MicrosoftReserved),
                (BASIC_DATA, Role::BasicData),
                (WINDOWS_RECOVERY, Role::WindowsRecovery),
                (LDM_METADATA, Role::LdmMetadata),
                (LDM_DATA, Role::LdmData),
                (STORAGE_SPACES, Role::StorageSpaces),
                (LINUX, Role::Linux),
                (LINUX_SWAP, Role::LinuxSwap),
                (LINUX_LVM, Role::LinuxLvm),
                (APFS, Role::Apfs),
                (HFS_PLUS, Role::HfsPlus),
            ]
            .into_iter()
            .find(|(guid, _)| guid == type_guid)
            .map(|(_, role)| role),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt(type_guid: Guid) -> Partition {
        Partition {
            number: 1,
            offset: 0,
            length: 1,
            kind: PartitionKind::Gpt {
                type_guid,
                unique_guid: Guid([0; 16]),
                attributes: 0,
                name: String::new(),
            },
        }
    }

    #[test]
    fn common_types_are_recognised() {
        assert_eq!(gpt(EFI_SYSTEM).role(), Some(Role::EfiSystem));
        assert_eq!(gpt(BASIC_DATA).role(), Some(Role::BasicData));
        assert_eq!(gpt(Guid([7; 16])).role(), None);
        let mbr = Partition {
            number: 1,
            offset: 0,
            length: 1,
            kind: PartitionKind::Mbr {
                type_id: 0x0C,
                bootable: false,
                logical: false,
            },
        };
        assert_eq!(mbr.role(), Some(Role::Fat32));
    }
}
