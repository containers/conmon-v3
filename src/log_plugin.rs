use libloading::{Library, Symbol};
use std::{
    env,
    ffi::{CString, OsStr},
    fs,
    os::raw::c_char,
    path::{Path, PathBuf},
};

use conmon_abi::{
    V1Getter,
    conmon_kv_t,
    conmon_log_plugin_v1,
    conmon_log_record_t,
    conmon_plugin
};

use crate::error::{ConmonError, ConmonResult};

#[cfg(target_os = "linux")]
const DYLIB_EXT: &str = "so";
#[cfg(target_os = "macos")]
const DYLIB_EXT: &str = "dylib";
#[cfg(target_os = "windows")]
const DYLIB_EXT: &str = "dll";

// Default plugin directories
const DEFAULT_LOG_PLUGIN_DIRS: &[&str] = &[
    "/usr/lib/conmon-v3/log_plugins",
    "/usr/local/lib/conmon-v3/log_plugins",
];

pub struct LoadedLogPlugin {
    _lib: Library,
    v1: &'static conmon_log_plugin_v1,
    handle: *mut conmon_plugin,
}

/// Convert libloading::Error to ConmonError.
impl From<libloading::Error> for ConmonError {
    fn from(e: libloading::Error) -> Self {
        ConmonError::new(e.to_string(), 1)
    }
}

fn file_exists(p: &Path) -> bool {
    match fs::metadata(p) {
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

/// Resolve plugin file:
/// 1) If `name_or_path` contains a path separator or ends with the dylib extension -> treat as path.
/// 2) Else search in: <exe-dir>, $CONMON_LOG_PLUGIN_PATH (':'-sep), DEFAULT_LOG_PLUGIN_DIRS.
///    Try filename `lib<name>_log_plugin.<ext>`
fn resolve_plugin_path(name_or_path: &str) -> Option<PathBuf> {
    let is_path_like = name_or_path.contains(std::path::MAIN_SEPARATOR)
        || Path::new(name_or_path).extension() == Some(OsStr::new(DYLIB_EXT));

    if is_path_like {
        let p = PathBuf::from(name_or_path);
        return p.exists().then_some(p);
    }

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    // Search for log plugins next to the conmon executable.
    let mut search_dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = exe_dir {
        search_dirs.push(d);
    }

    // Search for log plugins in the CONMON_LOG_PLUGIN_PATH.
    if let Ok(paths) = env::var("CONMON_LOG_PLUGIN_PATH") {
        for p in paths.split(':').filter(|s| !s.is_empty()) {
            search_dirs.push(PathBuf::from(p));
        }
    }

    // Search in the DEFAULT_LOG_PLUGIN_DIRS.
    for d in DEFAULT_LOG_PLUGIN_DIRS {
        search_dirs.push(PathBuf::from(d));
    }

    // Candidate plugin filename.
    let candidate = format!("lib{}_log_plugin.{}", name_or_path, DYLIB_EXT);

    for dir in search_dirs {
        let path = dir.join(&candidate);
        if file_exists(&path) {
            return Some(path);
        }
    }
    None
}

impl LoadedLogPlugin {
    pub fn load(name_or_path: &str, args: &[(&str, &str)]) -> ConmonResult<Self> {
        unsafe {
            // Get the path to plugin file.
            let path = resolve_plugin_path(name_or_path).ok_or_else(|| {
                ConmonError::new(
                    format!("Cannot load Log plugin {}: not found", name_or_path),
                    1,
                )
            })?;

            // Load the plugin.
            let lib = Library::new(path)?;
            let sym: Symbol<V1Getter> = lib.get(b"conmon_log_plugin_v1_get")?;
            let v1 = &*sym();

            // Check the ABI version and vtable size.
            if v1.abi_version != 1 {
                return Err(ConmonError::new(
                    format!("Cannot load Log plugin {}: ABI version mismatch", name_or_path),
                    1,
                ));
            }
            if (v1.struct_size as usize) < std::mem::size_of::<conmon_log_plugin_v1>() {
                return Err(ConmonError::new(
                    format!(
                        "Cannot load Log plugin {}: vtable struct too small",
                        name_or_path
                    ),
                    1,
                ));
            }

            // Marshal args.
            let c_kvs: Vec<conmon_kv_t> = args
                .iter()
                .map(|(k, v)| conmon_kv_t {
                    key: CString::new(*k).unwrap().into_raw(),
                    value: CString::new(*v).unwrap().into_raw(),
                })
                .collect();

            // Run the plugin's init function.
            let mut handle: *mut conmon_plugin = std::ptr::null_mut();
            let rc = (v1.init)(c_kvs.as_ptr(), c_kvs.len(), &mut handle);

            // reclaim CString memory immediately after init returns.
            for kv in &c_kvs {
                let _ = CString::from_raw(kv.key as *mut c_char);
                let _ = CString::from_raw(kv.value as *mut c_char);
            }

            // Return an error in case init failed to provide plugin handle.
            if rc != 0 || handle.is_null() {
                return Err(ConmonError::new(
                    format!("Cannot load Log plugin {}: init failed", name_or_path),
                    1,
                ));
            }

            Ok(Self {
                _lib: lib,
                v1,
                handle,
            })
        }
    }

    pub fn write(&self, rec: &conmon_log_record_t) -> i32 {
        unsafe { (self.v1.write)(self.handle, rec as *const _) }
    }
}

impl Drop for LoadedLogPlugin {
    fn drop(&mut self) {
        unsafe { (self.v1.close)(self.handle) }
    }
}
