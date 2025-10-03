#![deny(unsafe_op_in_unsafe_fn)]

use conmon_abi::{
    CONMON_LOG_OK, CONMON_LOG_PLUGIN_ABI_VERSION, conmon_kv_t, conmon_log_plugin_v1,
    conmon_log_record_t, conmon_plugin,
};
use std::os::raw::c_int;

struct NoneState;

/// Initialize the `none` logger.
/// C ABI: init(args,n_args,&out_handle) -> status
unsafe extern "C" fn v1_init(
    _args: *const conmon_kv_t,
    _n_args: usize,
    out_handle: *mut *mut conmon_plugin,
) -> c_int {
    // Allocate a trivial state so we have a valid, non-null handle.
    let state = Box::new(NoneState);
    unsafe { *out_handle = Box::into_raw(state) as *mut conmon_plugin };
    CONMON_LOG_OK
}

/// Writes record to log. Does nothing for `none` logger.
/// C ABI: write(handle, &record) -> status
unsafe extern "C" fn v1_write(
    _handle: *mut conmon_plugin,
    _rec: *const conmon_log_record_t,
) -> c_int {
    CONMON_LOG_OK
}

/// Closes and frees the resources.
/// C ABI: close(handle)
unsafe extern "C" fn v1_close(handle: *mut conmon_plugin) {
    if !handle.is_null() {
        // reclaim the Box allocated in init
        drop(unsafe { Box::from_raw(handle as *mut NoneState) });
    }
}

/// The  exported vtable symbol.
#[unsafe(no_mangle)]
pub extern "C" fn conmon_log_plugin_v1_get() -> *const conmon_log_plugin_v1 {
    // Static vtable with function pointers. flags: thread_safe bit set.
    static VTABLE: conmon_log_plugin_v1 = conmon_log_plugin_v1 {
        abi_version: CONMON_LOG_PLUGIN_ABI_VERSION,
        struct_size: std::mem::size_of::<conmon_log_plugin_v1>() as u32,
        flags: 0,
        init: v1_init,
        write: v1_write,
        close: v1_close,
    };
    &VTABLE
}
