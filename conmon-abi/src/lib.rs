#![no_std]
#![allow(non_camel_case_types)]

use core::ffi::c_char;
use core::ffi::c_void;

pub const CONMON_LOG_PLUGIN_ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct conmon_kv_t {
    pub key: *const c_char,   // null-terminated UTF-8
    pub value: *const c_char, // null-terminated UTF-8
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct conmon_log_record_t {
    pub stream: u32,
    pub data: *const u8,
    pub len: usize,
    pub flags: u32,
}

// Status codes
pub const CONMON_LOG_OK: i32 = 0;
pub const CONMON_LOG_ERR: i32 = -1;

// Forward-declared opaque in C
#[repr(C)]
pub struct conmon_plugin(c_void);

#[repr(C)]
#[derive(Debug)]
pub struct conmon_log_plugin_v1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub flags: u32,

    /// Initializes the log plugin.
    /// `args` - key, value pairs to configure the plugin.
    /// `n_args` - number of `args`.
    /// `out_handle` - handle holding the state of the plugin.
    /// Returns the CONMON_LOG_* status code.
    pub init: unsafe extern "C" fn(
        args: *const conmon_kv_t,
        n_args: usize,
        out_handle: *mut *mut conmon_plugin,
    ) -> i32,

    /// Writes the data to log.
    /// `handle` - handle holding the state of the plugin.
    /// `rec` - lod record to write to logs.
    /// Returns the CONMON_LOG_* status code.
    pub write:
        unsafe extern "C" fn(handle: *mut conmon_plugin, rec: *const conmon_log_record_t) -> i32,

    /// Closes and frees the resources.
    /// `handle` - handle holding the state of the plugin.
    pub close: unsafe extern "C" fn(handle: *mut conmon_plugin),
}

pub type V1Getter = unsafe extern "C" fn() -> *const conmon_log_plugin_v1;

// This is the ONLY required exported symbol from plugins.
// The implementation lives in each plugin crate; we just declare it here
// so cbindgen puts it into the header with the right signature.
unsafe extern "C" {
    pub fn conmon_log_plugin_v1_get() -> *const conmon_log_plugin_v1;
}
