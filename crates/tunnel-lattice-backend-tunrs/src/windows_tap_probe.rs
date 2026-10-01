//! Detects whether the tap-windows6 driver (hardware id `tap0901`) is
//! installed, without creating or registering any device.
//!
//! `tun-rs` 2.8.11 has no probe API. Its `open(Tap)` path
//! (`platform/windows/tap/iface.rs`, `create_interface`) builds an
//! in-memory network-class device element, sets its hardware id to
//! `tap0901`, asks SetupAPI for the compatible drivers, and fails with
//! `"No driver found"` unless one of them has that hardware id, a non-zero
//! `DriverVersion`, and can be selected for the element
//! (`SetupDiSetSelectedDriverW`). This module performs the same lookup with
//! the same hardware-id and `DriverVersion` checks, but does not select a
//! driver: selection only marks the driver on this in-memory element, and
//! skipping it keeps the probe free of any state change. So the probe can
//! report a driver that `tun-rs` would then fail to select; that mismatch is
//! not expected for a staged driver package, and `open` stays authoritative
//! either way. The probe never calls the class installer
//! (`DIF_REGISTERDEVICE`/`DIF_INSTALLDEVICE`), so the element only ever
//! exists inside the device information set, which is destroyed before
//! returning. None of these calls needs elevation.
//!
//! Every failure is treated as "not installed": the answer feeds
//! `Capability::TAP_DEVICES`, which is advisory (`open` stays
//! authoritative).

use std::sync::OnceLock;
use std::{mem, ptr};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, GUID_DEVCLASS_NET, HDEVINFO, MAX_CLASS_NAME_LEN, SP_DEVINFO_DATA,
    SP_DRVINFO_DATA_V2_W, SP_DRVINFO_DETAIL_DATA_W, SPDIT_COMPATDRIVER, SPDRP_HARDWAREID,
    SetupDiBuildDriverInfoList, SetupDiClassNameFromGuidW, SetupDiCreateDeviceInfoList,
    SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList, SetupDiDestroyDriverInfoList,
    SetupDiEnumDriverInfoW, SetupDiGetDriverInfoDetailW, SetupDiSetDeviceRegistryPropertyW,
    SetupDiSetSelectedDevice,
};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, GetLastError, INVALID_HANDLE_VALUE};

/// The tap-windows6 hardware id `tun-rs` 2.8.11 installs adapters with.
pub(crate) const TAP_HARDWARE_ID: &str = "tap0901";

/// Upper bound on the compatible drivers inspected, so a corrupt or
/// unusually large driver store cannot make the probe unbounded.
const MAX_DRIVERS: u32 = 1024;

/// Initial size, in bytes, of the driver detail buffer: the fixed part plus
/// room for the hardware id and compatible ids that follow it.
const DETAIL_BUFFER_BYTES: usize = 4096;

/// Largest driver detail buffer the probe will allocate after SetupAPI
/// reports the first one was too small.
const MAX_DETAIL_BUFFER_BYTES: usize = 64 * 1024;

/// Whether the `tap0901` driver is installed, probed on the first call and
/// cached for the life of the process.
///
/// A driver installed or removed after the first call is not noticed until
/// the process restarts.
pub(crate) fn tap_driver_installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(probe_tap_driver)
}

/// Destroys the device information set on every exit path.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a set returned by `SetupDiCreateDeviceInfoList`
        // (checked against `INVALID_HANDLE_VALUE` before this guard is
        // built) and is destroyed exactly once, here. Destroying it also
        // frees the in-memory device element created in it.
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Destroys the compatible-driver list built for one device element on
/// every exit path. Declared after the [`DeviceInfoSet`] guard in
/// [`probe_tap_driver`], so it drops first.
struct DriverInfoList<'a> {
    set: HDEVINFO,
    device: &'a SP_DEVINFO_DATA,
}

impl Drop for DriverInfoList<'_> {
    fn drop(&mut self) {
        // SAFETY: `SetupDiBuildDriverInfoList` succeeded for this set and
        // element before the guard was built; both are still alive (the set
        // guard drops after this one, and `device` is borrowed for this
        // guard's lifetime). The list is destroyed exactly once, here.
        unsafe {
            SetupDiDestroyDriverInfoList(self.set, self.device, SPDIT_COMPATDRIVER);
        }
    }
}

/// Runs the probe once; see the module documentation.
fn probe_tap_driver() -> bool {
    // SAFETY: the class GUID is a valid static, and a null parent window is
    // allowed. The returned set is checked before use and destroyed by
    // `DeviceInfoSet`.
    let raw = unsafe { SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_NET, ptr::null_mut()) };
    if raw == INVALID_HANDLE_VALUE as HDEVINFO {
        return false;
    }
    let set = DeviceInfoSet(raw);

    let mut class_name = [0u16; MAX_CLASS_NAME_LEN as usize];
    // SAFETY: `class_name` is a writable buffer of exactly the length
    // passed; a null required-size pointer is allowed. On success the
    // buffer holds a NUL-terminated name.
    let ok = unsafe {
        SetupDiClassNameFromGuidW(
            &GUID_DEVCLASS_NET,
            class_name.as_mut_ptr(),
            MAX_CLASS_NAME_LEN,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return false;
    }

    let description = [0u16; 1];
    let mut device = SP_DEVINFO_DATA {
        cbSize: mem::size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    // SAFETY: `set.0` is a live set; `class_name` and `description` are
    // NUL-terminated UTF-16 strings that outlive the call; `device` is a
    // writable, correctly sized `SP_DEVINFO_DATA`. `DICD_GENERATE_ID`
    // creates an in-memory element with a generated instance id only: it is
    // registered with the PnP manager solely by a later
    // `DIF_REGISTERDEVICE`, which this module never issues.
    let ok = unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            class_name.as_ptr(),
            &GUID_DEVCLASS_NET,
            description.as_ptr(),
            ptr::null_mut(),
            DICD_GENERATE_ID,
            &mut device,
        )
    };
    if ok == 0 {
        return false;
    }

    // SAFETY: `device` was just filled in for an element of `set.0`.
    if unsafe { SetupDiSetSelectedDevice(set.0, &device) } == 0 {
        return false;
    }

    // `SPDRP_HARDWAREID` is a REG_MULTI_SZ: the id, its NUL, and a final
    // NUL ending the list.
    let hardware_id: Vec<u16> = TAP_HARDWARE_ID.encode_utf16().chain([0, 0]).collect();
    let Ok(hardware_id_bytes) = u32::try_from(hardware_id.len() * mem::size_of::<u16>()) else {
        return false;
    };
    // SAFETY: `device` belongs to `set.0`; the property buffer points to
    // `hardware_id_bytes` readable bytes that outlive the call. On an
    // element that was never registered this only updates the in-memory
    // element, as `tun-rs` relies on.
    let ok = unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            set.0,
            &mut device,
            SPDRP_HARDWAREID,
            hardware_id.as_ptr().cast(),
            hardware_id_bytes,
        )
    };
    if ok == 0 {
        return false;
    }

    // SAFETY: `device` belongs to `set.0`. The list built here is destroyed
    // by the `DriverInfoList` guard below.
    if unsafe { SetupDiBuildDriverInfoList(set.0, &mut device, SPDIT_COMPATDRIVER) } == 0 {
        return false;
    }
    let drivers = DriverInfoList {
        set: set.0,
        device: &device,
    };

    let found = (0..MAX_DRIVERS)
        .map_while(|index| enum_driver(&drivers, index))
        .flatten()
        // `tun-rs` only accepts a driver whose version is above the best one
        // so far, starting from 0, so a zero `DriverVersion` never counts.
        .filter(|driver| driver.DriverVersion != 0)
        .any(|driver| {
            driver_hardware_id(&drivers, &driver)
                .is_some_and(|id| id.eq_ignore_ascii_case(TAP_HARDWARE_ID))
        });
    drop(drivers);
    drop(set);
    found
}

/// Returns `None` once the enumeration is exhausted, `Some(None)` for an
/// entry that could not be read (skipped, as `tun-rs` does), and
/// `Some(Some(_))` for a driver.
fn enum_driver(drivers: &DriverInfoList<'_>, index: u32) -> Option<Option<SP_DRVINFO_DATA_V2_W>> {
    let mut driver = SP_DRVINFO_DATA_V2_W {
        cbSize: mem::size_of::<SP_DRVINFO_DATA_V2_W>() as u32,
        ..Default::default()
    };
    // SAFETY: the set, its element, and its compatible-driver list are
    // alive for the guard's lifetime; `driver` is writable and correctly
    // sized.
    let ok = unsafe {
        SetupDiEnumDriverInfoW(
            drivers.set,
            drivers.device,
            SPDIT_COMPATDRIVER,
            index,
            &mut driver,
        )
    };
    if ok != 0 {
        return Some(Some(driver));
    }
    // SAFETY: reads the calling thread's last-error value; no arguments.
    if unsafe { GetLastError() } == ERROR_NO_MORE_ITEMS {
        None
    } else {
        Some(None)
    }
}

/// Reads one compatible driver's hardware id, or `None` if it cannot be
/// read.
fn driver_hardware_id(
    drivers: &DriverInfoList<'_>,
    driver: &SP_DRVINFO_DATA_V2_W,
) -> Option<String> {
    let fixed = mem::size_of::<SP_DRVINFO_DETAIL_DATA_W>();
    let hardware_id_offset = mem::offset_of!(SP_DRVINFO_DETAIL_DATA_W, HardwareID);
    let mut bytes = DETAIL_BUFFER_BYTES;
    // At most two attempts: the first buffer, then one of the size SetupAPI
    // reports as required.
    for _ in 0..2 {
        // `u64` elements give the buffer the 8-byte alignment the struct
        // needs on 64-bit targets (it is packed on 32-bit x86).
        let mut buffer = vec![0u64; bytes.div_ceil(mem::size_of::<u64>())];
        let capacity = buffer.len() * mem::size_of::<u64>();
        let detail = buffer.as_mut_ptr().cast::<SP_DRVINFO_DETAIL_DATA_W>();
        // SAFETY: `cbSize` is the first field (offset 0) and the buffer is
        // at least `fixed` bytes and 8-byte aligned, so the write is in
        // bounds; `write_unaligned` also covers the packed x86 layout.
        unsafe {
            ptr::addr_of_mut!((*detail).cbSize).write_unaligned(fixed as u32);
        }
        let mut required = 0u32;
        // SAFETY: the set, element, and driver list are alive; `driver` was
        // returned by `SetupDiEnumDriverInfoW` for them; `detail` points to
        // `capacity` writable bytes, which is the size passed.
        let ok = unsafe {
            SetupDiGetDriverInfoDetailW(
                drivers.set,
                drivers.device,
                driver,
                detail,
                u32::try_from(capacity).ok()?,
                &mut required,
            )
        };
        if ok != 0 {
            let units = (capacity - hardware_id_offset) / mem::size_of::<u16>();
            // SAFETY: `hardware_id_offset..capacity` lies inside `buffer`,
            // which SetupAPI filled; the offset is even, so the slice of
            // `u16` is aligned. The hardware id is NUL-terminated within it
            // (the compatible ids that follow are only read up to that NUL).
            let tail = unsafe {
                std::slice::from_raw_parts(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(hardware_id_offset)
                        .cast::<u16>(),
                    units,
                )
            };
            let end = tail.iter().position(|&unit| unit == 0)?;
            return Some(String::from_utf16_lossy(&tail[..end]));
        }
        let required = required as usize;
        if required <= capacity || required > MAX_DETAIL_BUFFER_BYTES {
            return None;
        }
        bytes = required;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe needs no elevation and never panics; its answer is cached,
    /// so a second call agrees with the first. Whether the driver is
    /// installed depends on the host, so the value itself is not asserted.
    #[test]
    fn tap_driver_probe_is_cached_and_does_not_panic() {
        let first = tap_driver_installed();
        assert_eq!(tap_driver_installed(), first);
    }

    #[test]
    fn hardware_id_matches_tun_rs() {
        assert_eq!(TAP_HARDWARE_ID, "tap0901");
    }
}
