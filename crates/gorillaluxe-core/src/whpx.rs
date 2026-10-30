//! Windows Hypervisor Platform (WHPX) bindings, resolved at runtime.
//!
//! WinHvPlatform.dll is loaded dynamically so hosts without the feature
//! get a clean `WhpxError::NotInstalled` instead of a loader failure.

use std::ffi::c_void;
use std::fmt;
use std::mem::size_of;
use std::sync::OnceLock;

use windows::core::HRESULT;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Hypervisor::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type GetCapabilityFn =
    unsafe extern "system" fn(WHV_CAPABILITY_CODE, *mut c_void, u32, *mut u32) -> HRESULT;
type CreatePartitionFn = unsafe extern "system" fn(*mut WHV_PARTITION_HANDLE) -> HRESULT;
type SetupPartitionFn = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
type DeletePartitionFn = unsafe extern "system" fn(WHV_PARTITION_HANDLE) -> HRESULT;
type SetPartitionPropertyFn = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    WHV_PARTITION_PROPERTY_CODE,
    *const c_void,
    u32,
) -> HRESULT;
type MapGpaRangeFn = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    *const c_void,
    u64,
    u32,
    WHV_MAP_GPA_RANGE_FLAGS,
) -> HRESULT;
type CreateVirtualProcessorFn =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, u32) -> HRESULT;
type DeleteVirtualProcessorFn = unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32) -> HRESULT;
type RunVirtualProcessorFn =
    unsafe extern "system" fn(WHV_PARTITION_HANDLE, u32, *mut c_void, u32) -> HRESULT;
type SetVpRegistersFn = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *const WHV_REGISTER_VALUE,
) -> HRESULT;
type GetVpRegistersFn = unsafe extern "system" fn(
    WHV_PARTITION_HANDLE,
    u32,
    *const WHV_REGISTER_NAME,
    u32,
    *mut WHV_REGISTER_VALUE,
) -> HRESULT;

#[derive(Debug, Clone)]
pub enum WhpxError {
    /// WinHvPlatform.dll could not be loaded (feature not installed).
    NotInstalled,
    MissingExport(&'static str),
    ApiFailed(&'static str, i32),
}

impl fmt::Display for WhpxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WhpxError::NotInstalled => write!(f, "WinHvPlatform.dll is not available"),
            WhpxError::MissingExport(s) => write!(f, "WinHvPlatform.dll is missing export {}", s),
            WhpxError::ApiFailed(s, hr) => write!(f, "{} failed (HRESULT 0x{:08X})", s, hr),
        }
    }
}

impl std::error::Error for WhpxError {}

/// Lazily resolved WinHvPlatform.dll entry points.
pub struct Whpx {
    get_capability: GetCapabilityFn,
    create_partition: CreatePartitionFn,
    setup_partition: SetupPartitionFn,
    delete_partition: DeletePartitionFn,
    set_partition_property: SetPartitionPropertyFn,
    map_gpa_range: MapGpaRangeFn,
    create_virtual_processor: CreateVirtualProcessorFn,
    delete_virtual_processor: DeleteVirtualProcessorFn,
    run_virtual_processor: RunVirtualProcessorFn,
    set_vp_registers: SetVpRegistersFn,
    get_vp_registers: GetVpRegistersFn,
}

impl Whpx {
    /// Resolve the WHPX API once per process.
    pub fn instance() -> Result<&'static Whpx, WhpxError> {
        static CELL: OnceLock<Result<Whpx, WhpxError>> = OnceLock::new();
        match CELL.get_or_init(Whpx::load) {
            Ok(api) => Ok(api),
            Err(e) => Err(e.clone()),
        }
    }

    fn load() -> Result<Whpx, WhpxError> {
        let lib = unsafe { LoadLibraryW(windows::core::w!("WinHvPlatform.dll")) }
            .map_err(|_| WhpxError::NotInstalled)?;

        unsafe fn sym<T>(lib: HMODULE, name: &'static [u8]) -> Result<T, WhpxError> {
            let pcstr = windows::core::PCSTR(name.as_ptr());
            let proc = unsafe { GetProcAddress(lib, pcstr) }.ok_or(WhpxError::MissingExport(
                std::str::from_utf8(name)
                    .unwrap_or("?")
                    .trim_end_matches('\0'),
            ))?;
            Ok(unsafe { std::mem::transmute_copy(&proc) })
        }

        let api = unsafe {
            Whpx {
                get_capability: sym(lib, b"WHvGetCapability\0")?,
                create_partition: sym(lib, b"WHvCreatePartition\0")?,
                setup_partition: sym(lib, b"WHvSetupPartition\0")?,
                delete_partition: sym(lib, b"WHvDeletePartition\0")?,
                set_partition_property: sym(lib, b"WHvSetPartitionProperty\0")?,
                map_gpa_range: sym(lib, b"WHvMapGpaRange\0")?,
                create_virtual_processor: sym(lib, b"WHvCreateVirtualProcessor\0")?,
                delete_virtual_processor: sym(lib, b"WHvDeleteVirtualProcessor\0")?,
                run_virtual_processor: sym(lib, b"WHvRunVirtualProcessor\0")?,
                set_vp_registers: sym(lib, b"WHvSetVirtualProcessorRegisters\0")?,
                get_vp_registers: sym(lib, b"WHvGetVirtualProcessorRegisters\0")?,
            }
        };
        // The DLL stays loaded for the process lifetime (HMODULE is never freed).
        Ok(api)
    }

    /// WHvCapabilityCodeHypervisorPresent.
    pub fn hypervisor_present(&self) -> Result<bool, WhpxError> {
        let mut cap = WHV_CAPABILITY::default();
        let mut written = 0u32;
        let hr = unsafe {
            (self.get_capability)(
                WHvCapabilityCodeHypervisorPresent,
                &mut cap as *mut _ as *mut c_void,
                size_of::<WHV_CAPABILITY>() as u32,
                &mut written,
            )
        };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvGetCapability", hr.0));
        }
        Ok(unsafe { cap.HypervisorPresent.as_bool() })
    }

    /// Create + configure + set up a partition with `vcpu_count` vCPUs.
    pub fn create_partition(&'static self, vcpu_count: u32) -> Result<Partition, WhpxError> {
        let mut handle = WHV_PARTITION_HANDLE::default();
        let hr = unsafe { (self.create_partition)(&mut handle) };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvCreatePartition", hr.0));
        }

        let mut prop = WHV_PARTITION_PROPERTY::default();
        prop.ProcessorCount = vcpu_count;
        let hr = unsafe {
            (self.set_partition_property)(
                handle,
                WHvPartitionPropertyCodeProcessorCount,
                &prop as *const _ as *const c_void,
                size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        };
        if hr.is_err() {
            unsafe {
                let _ = (self.delete_partition)(handle);
            }
            return Err(WhpxError::ApiFailed("WHvSetPartitionProperty", hr.0));
        }

        let hr = unsafe { (self.setup_partition)(handle) };
        if hr.is_err() {
            unsafe {
                let _ = (self.delete_partition)(handle);
            }
            return Err(WhpxError::ApiFailed("WHvSetupPartition", hr.0));
        }
        Ok(Partition { handle, api: self })
    }
}

/// RAII wrapper over a WHPX partition handle.
pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
    api: &'static Whpx,
}

impl Partition {
    pub fn handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }

    /// # Safety
    /// `host_addr` must point at `size` bytes of valid host memory for
    /// the lifetime of the GPA mapping.
    pub unsafe fn map_gpa_range(
        &self,
        host_addr: *const c_void,
        gpa: u64,
        size: u32,
        flags: WHV_MAP_GPA_RANGE_FLAGS,
    ) -> Result<(), WhpxError> {
        let hr = unsafe { (self.api.map_gpa_range)(self.handle, host_addr, gpa, size, flags) };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvMapGpaRange", hr.0));
        }
        Ok(())
    }

    pub fn create_vcpu(&self, index: u32) -> Result<(), WhpxError> {
        let hr = unsafe { (self.api.create_virtual_processor)(self.handle, index, 0) };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvCreateVirtualProcessor", hr.0));
        }
        Ok(())
    }

    pub fn delete_vcpu(&self, index: u32) -> Result<(), WhpxError> {
        let hr = unsafe { (self.api.delete_virtual_processor)(self.handle, index) };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvDeleteVirtualProcessor", hr.0));
        }
        Ok(())
    }

    pub fn set_registers(
        &self,
        vp: u32,
        names: &[WHV_REGISTER_NAME],
        values: &[WHV_REGISTER_VALUE],
    ) -> Result<(), WhpxError> {
        assert_eq!(names.len(), values.len());
        let hr = unsafe {
            (self.api.set_vp_registers)(
                self.handle,
                vp,
                names.as_ptr(),
                names.len() as u32,
                values.as_ptr(),
            )
        };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed(
                "WHvSetVirtualProcessorRegisters",
                hr.0,
            ));
        }
        Ok(())
    }

    pub fn get_registers(
        &self,
        vp: u32,
        names: &[WHV_REGISTER_NAME],
    ) -> Result<Vec<WHV_REGISTER_VALUE>, WhpxError> {
        let mut values = vec![WHV_REGISTER_VALUE::default(); names.len()];
        let hr = unsafe {
            (self.api.get_vp_registers)(
                self.handle,
                vp,
                names.as_ptr(),
                names.len() as u32,
                values.as_mut_ptr(),
            )
        };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed(
                "WHvGetVirtualProcessorRegisters",
                hr.0,
            ));
        }
        Ok(values)
    }

    /// Run vCPU once; fills `exit` with the exit context on VM-exit.
    pub fn run(&self, vp: u32, exit: &mut WHV_RUN_VP_EXIT_CONTEXT) -> Result<(), WhpxError> {
        let hr = unsafe {
            (self.api.run_virtual_processor)(
                self.handle,
                vp,
                exit as *mut _ as *mut c_void,
                size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };
        if hr.is_err() {
            return Err(WhpxError::ApiFailed("WHvRunVirtualProcessor", hr.0));
        }
        Ok(())
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.api.delete_partition)(self.handle);
        }
    }
}
