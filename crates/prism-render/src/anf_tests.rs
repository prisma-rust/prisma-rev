//! ANF 单元测试（#1161）。

use ash::vk;
use super::anf::{is_qualcomm_adreno, AnfCaps, QUALCOMM_VENDOR_ID};

#[test]
fn test_is_qualcomm_adreno_by_vendor_id() {
    let mut props = vk::PhysicalDeviceProperties::default();
    props.vendor_id = QUALCOMM_VENDOR_ID;
    assert!(is_qualcomm_adreno(&props));

    let caps = AnfCaps::probe(&props);
    assert!(caps.is_adreno);
    assert!(caps.sr_supported);
    assert!(caps.fg_supported);
}

#[test]
fn test_is_qualcomm_adreno_by_device_name() {
    let mut props = vk::PhysicalDeviceProperties::default();
    props.vendor_id = 0x10DE; // NVIDIA vendor ID
    let name = b"Adreno (TM) 850\0";
    for (i, &b) in name.iter().enumerate() {
        props.device_name[i] = b as std::os::raw::c_char;
    }
    assert!(is_qualcomm_adreno(&props));

    let caps = AnfCaps::probe(&props);
    assert!(caps.is_adreno);
    assert!(caps.sr_supported);
}

#[test]
fn test_non_adreno_device() {
    let mut props = vk::PhysicalDeviceProperties::default();
    props.vendor_id = 0x8086; // Intel
    let name = b"Intel(R) UHD Graphics\0";
    for (i, &b) in name.iter().enumerate() {
        props.device_name[i] = b as std::os::raw::c_char;
    }
    assert!(!is_qualcomm_adreno(&props));

    let caps = AnfCaps::probe(&props);
    assert!(!caps.is_adreno);
    assert!(!caps.sr_supported);
    assert!(!caps.fg_supported);
}
