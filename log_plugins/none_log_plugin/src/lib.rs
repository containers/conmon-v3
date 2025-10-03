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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{mem, ptr};

    #[test]
    fn vtable_metadata_is_valid() {
        unsafe {
            let v = &*conmon_log_plugin_v1_get();
            assert_eq!(v.abi_version, CONMON_LOG_PLUGIN_ABI_VERSION);
            assert!((v.struct_size as usize) >= mem::size_of::<conmon_log_plugin_v1>());

            // sanity: function pointers wired up to our functions
            assert_eq!(v.init as usize, v1_init as usize);
            assert_eq!(v.write as usize, v1_write as usize);
            assert_eq!(v.close as usize, v1_close as usize);
        }
    }

    #[test]
    fn init_write_close_roundtrip_ok() {
        unsafe {
            let v = &*conmon_log_plugin_v1_get();

            // init with no args
            let mut handle: *mut conmon_plugin = ptr::null_mut();
            let rc = (v.init)(ptr::null(), 0, &mut handle as *mut _);
            assert_eq!(rc, CONMON_LOG_OK);
            assert!(!handle.is_null());

            // write a record (contents are ignored by this logger)
            // Use zeroed to produce a valid-by-construction C struct value.
            let rec: conmon_log_record_t = mem::zeroed();
            let rcw = (v.write)(handle, &rec as *const _);
            assert_eq!(rcw, CONMON_LOG_OK);

            // close the valid handle
            (v.close)(handle);

            // close should be a no-op for null
            (v.close)(ptr::null_mut());
        }
    }
}
