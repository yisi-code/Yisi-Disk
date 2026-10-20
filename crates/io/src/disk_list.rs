//! 列出物理磁盘：以零访问权限打开设备，只查询属性、不读磁盘内容，所以不需要管理员权限。

use std::fs::OpenOptions;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    BusTypeAta, BusTypeFileBackedVirtual, BusTypeMmc, BusTypeNvme, BusTypeSas, BusTypeSata,
    BusTypeScsi, BusTypeSd, BusTypeUsb, BusTypeVirtual, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};

use crate::ioctl::{field, ioctl};

/// 检查 `\\.\PhysicalDrive0` 到这个编号之前的全部设备；拔掉过的盘会留下编号空缺，所以不在第一个空缺处停。
const MAX_DISKS: u32 = 64;
/// 查询结果的缓冲区：几何信息之后可能附带分区与检测信息，设备描述符之后是若干字符串。
const QUERY_BUFFER_SIZE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusType {
    Ata,
    Sata,
    Scsi,
    Sas,
    Nvme,
    Usb,
    Sd,
    Mmc,
    Virtual,
    Other,
}

/// 一块物理磁盘的基本信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub number: u32,
    /// `\\.\PhysicalDriveN`，交给磁盘代理打开。
    pub path: String,
    pub size: u64,
    /// 逻辑扇区大小。
    pub sector_size: u32,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    pub bus: BusType,
    pub removable: bool,
}

/// 列出能查到容量的物理磁盘，按编号排序。打不开、或查不到容量的设备（例如没插卡的读卡器）跳过。
pub fn list_disks() -> Vec<DiskInfo> {
    (0..MAX_DISKS).filter_map(query_disk).collect()
}

fn query_disk(number: u32) -> Option<DiskInfo> {
    let path = format!(r"\\.\PhysicalDrive{number}");
    let device = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .ok()?;
    let handle = device.as_raw_handle();

    let mut geometry = [0u8; QUERY_BUFFER_SIZE];
    let returned = ioctl(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry).ok()?;
    let geometry = geometry.get(..returned)?;
    let size = i64::from_ne_bytes(field(geometry, offset_of!(DISK_GEOMETRY_EX, DiskSize)).ok()?);
    let sector_size = u32::from_ne_bytes(
        field(
            geometry,
            offset_of!(DISK_GEOMETRY_EX, Geometry) + offset_of!(DISK_GEOMETRY, BytesPerSector),
        )
        .ok()?,
    );

    let mut query = [0u8; size_of::<STORAGE_PROPERTY_QUERY>()];
    write_i32(
        &mut query,
        offset_of!(STORAGE_PROPERTY_QUERY, PropertyId),
        StorageDeviceProperty,
    );
    write_i32(
        &mut query,
        offset_of!(STORAGE_PROPERTY_QUERY, QueryType),
        PropertyStandardQuery,
    );
    let mut descriptor = [0u8; QUERY_BUFFER_SIZE];
    // 有的设备不支持这项查询，此时型号等留空，仍列出这块盘
    let described = ioctl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &query,
        &mut descriptor,
    )
    .ok()
    .and_then(|returned| parse_device_descriptor(descriptor.get(..returned)?));
    let described = described.unwrap_or_default();

    Some(DiskInfo {
        number,
        path,
        size: u64::try_from(size).ok()?,
        sector_size,
        vendor: described.vendor,
        model: described.model,
        serial: described.serial,
        bus: described.bus,
        removable: described.removable,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct DeviceDescriptor {
    vendor: String,
    model: String,
    serial: String,
    bus: BusType,
    removable: bool,
}

impl Default for DeviceDescriptor {
    fn default() -> Self {
        Self {
            vendor: String::new(),
            model: String::new(),
            serial: String::new(),
            bus: BusType::Other,
            removable: false,
        }
    }
}

/// 解析 `STORAGE_DEVICE_DESCRIPTOR`。字符串字段是相对缓冲区开头的偏移，0 表示没有；
/// 偏移越界或字符串缺少结尾时按缓冲区截断，不读缓冲区以外的内容。
fn parse_device_descriptor(bytes: &[u8]) -> Option<DeviceDescriptor> {
    let offset_field = |offset: usize| -> Option<usize> {
        usize::try_from(u32::from_ne_bytes(field(bytes, offset).ok()?)).ok()
    };
    let vendor = offset_field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset))?;
    let model = offset_field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset))?;
    let serial = offset_field(offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset))?;
    let bus =
        i32::from_ne_bytes(field(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType)).ok()?);
    let removable = *bytes.get(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia))? != 0;
    Some(DeviceDescriptor {
        vendor: string_at(bytes, vendor),
        model: string_at(bytes, model),
        serial: string_at(bytes, serial),
        bus: bus_type(bus),
        removable,
    })
}

fn string_at(bytes: &[u8], offset: usize) -> String {
    if offset == 0 {
        return String::new();
    }
    let Some(tail) = bytes.get(offset..) else {
        return String::new();
    };
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..end]).trim().to_owned()
}

/// `STORAGE_BUS_TYPE` 的取值与本 crate 总线类型的对应；表里没有的一律算其他。
const BUS_TYPES: [(i32, BusType); 10] = [
    (BusTypeAta, BusType::Ata),
    (BusTypeSata, BusType::Sata),
    (BusTypeScsi, BusType::Scsi),
    (BusTypeSas, BusType::Sas),
    (BusTypeNvme, BusType::Nvme),
    (BusTypeUsb, BusType::Usb),
    (BusTypeSd, BusType::Sd),
    (BusTypeMmc, BusType::Mmc),
    (BusTypeVirtual, BusType::Virtual),
    (BusTypeFileBackedVirtual, BusType::Virtual),
];

fn bus_type(value: i32) -> BusType {
    BUS_TYPES
        .iter()
        .find(|(raw, _)| *raw == value)
        .map_or(BusType::Other, |(_, bus)| *bus)
}

fn write_i32(buffer: &mut [u8], offset: usize, value: i32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 `STORAGE_DEVICE_DESCRIPTOR` 的布局拼一份描述符，字符串依次接在结构体后面。
    fn descriptor(vendor: &str, model: &str, serial: &str, bus: i32, removable: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; size_of::<STORAGE_DEVICE_DESCRIPTOR>()];
        let place = |bytes: &mut Vec<u8>, offset_field: usize, text: &str| {
            if text.is_empty() {
                return;
            }
            let at = bytes.len() as u32;
            bytes[offset_field..offset_field + 4].copy_from_slice(&at.to_ne_bytes());
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        };
        place(
            &mut bytes,
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset),
            vendor,
        );
        place(
            &mut bytes,
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset),
            model,
        );
        place(
            &mut bytes,
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset),
            serial,
        );
        write_i32(
            &mut bytes,
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType),
            bus,
        );
        bytes[offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia)] = u8::from(removable);
        bytes
    }

    #[test]
    fn parses_strings_bus_and_removable_flag() {
        let bytes = descriptor("FIKWOT  ", "FP80", "0123ABCD", BusTypeUsb, true);
        assert_eq!(
            parse_device_descriptor(&bytes),
            Some(DeviceDescriptor {
                vendor: "FIKWOT".to_owned(),
                model: "FP80".to_owned(),
                serial: "0123ABCD".to_owned(),
                bus: BusType::Usb,
                removable: true,
            })
        );
    }

    #[test]
    fn missing_strings_are_empty() {
        let parsed =
            parse_device_descriptor(&descriptor("", "CT1000P310SSD8", "", BusTypeNvme, false))
                .unwrap();
        assert_eq!(
            (parsed.vendor.as_str(), parsed.model.as_str()),
            ("", "CT1000P310SSD8")
        );
        assert_eq!(parsed.bus, BusType::Nvme);
    }

    #[test]
    fn out_of_range_offsets_and_unterminated_strings_stay_inside_buffer() {
        let mut bytes = descriptor("", "MODEL", "", 99, false);
        // 型号缺少结尾的 0：截到缓冲区末尾
        bytes.pop();
        let vendor_field = offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset);
        bytes[vendor_field..vendor_field + 4].copy_from_slice(&u32::MAX.to_ne_bytes());
        let parsed = parse_device_descriptor(&bytes).unwrap();
        assert_eq!(parsed.vendor, "");
        assert_eq!(parsed.model, "MODEL");
        assert_eq!(parsed.bus, BusType::Other);
    }

    #[test]
    fn truncated_descriptor_is_rejected() {
        let bytes = descriptor("V", "M", "S", BusTypeSata, false);
        assert!(parse_device_descriptor(&bytes[..8]).is_none());
    }
}
