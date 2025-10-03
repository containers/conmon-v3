use libloading::{Library, Symbol};
use std::{
    env,
    ffi::OsStr,
    os::raw::c_char,
    path::{Path, PathBuf},
};

use conmon_abi::{V1Getter, conmon_kv_t, conmon_log_plugin_v1, conmon_log_record_t, conmon_plugin};

use crate::error::{ConmonError, ConmonResult};

const DYLIB_EXT: &str = "so";

// Default plugin directories
const DEFAULT_LOG_PLUGIN_DIRS: &[&str] = &[
    "/usr/lib/conmon-v3/log_plugins",
    "/usr/local/lib/conmon-v3/log_plugins",
];

#[derive(Debug)]
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

/// Resolve plugin file:
/// 1) If `name_or_path` contains directories or ends with the dylib extension -> treat as path.
/// 2) Else search in: <exe-dir>, $CONMON_LOG_PLUGIN_PATH (':'-sep), DEFAULT_LOG_PLUGIN_DIRS.
///    Try filename `lib<name>_log_plugin.<ext>`
fn resolve_plugin_path(name_or_path: &str) -> Option<PathBuf> {
    // If the name_or_path is a path, return it if it exists.
    let p = Path::new(name_or_path);
    if matches!(p.parent(), Some(par) if !par.as_os_str().is_empty())
        || p.extension()
            .is_some_and(|ext| ext == OsStr::new(DYLIB_EXT))
    {
        return p.exists().then_some(p.to_path_buf());
    }

    // Get the path to our conmon executable.
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
        if path.is_file() {
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
                    format!(
                        "Cannot load Log plugin {}: ABI version mismatch",
                        name_or_path
                    ),
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
                    key: k.as_ptr() as *const c_char,
                    value: v.as_ptr() as *const c_char,
                })
                .collect();

            // Run the plugin's init function.
            let mut handle: *mut conmon_plugin = std::ptr::null_mut();
            let rc = (v1.init)(c_kvs.as_ptr(), c_kvs.len(), &mut handle);

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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn resolve_path_accepts_explicit_existing_path() -> ConmonResult<()> {
        let dir = tempdir()?;
        let p = dir
            .path()
            .join(format!("libdummy_log_plugin.{}", DYLIB_EXT));
        fs::write(&p, b"not a real so, just exists")?;

        // Path-like because it has the extension.
        let got = resolve_plugin_path(&p.to_string_lossy());
        assert_eq!(got.as_deref(), Some(p.as_path()));
        Ok(())
    }

    #[test]
    #[serial]
    fn resolve_path_rejects_nonexistent_explicit_path() -> ConmonResult<()> {
        let dir = tempdir()?;
        let p = dir
            .path()
            .join(format!("libmissing_log_plugin.{}", DYLIB_EXT));
        // Path does not exist.
        assert!(resolve_plugin_path(&p.to_string_lossy()).is_none());
        Ok(())
    }

    // This test uses environment variables; serialize it with others to avoid cross-test races.
    #[test]
    #[serial]
    fn resolve_searches_conmon_log_plugin_path_single_dir() -> ConmonResult<()> {
        let dir = tempdir()?;
        let name = "demo";
        let plugin = dir
            .path()
            .join(format!("lib{}_log_plugin.{}", name, DYLIB_EXT));
        fs::write(&plugin, b"exists")?;

        // Point search path to our temp dir
        unsafe {
            std::env::set_var("CONMON_LOG_PLUGIN_PATH", dir.path());
        }
        // make sure no accidental hit from exe-dir or defaults
        let got = resolve_plugin_path(name);
        unsafe {
            std::env::remove_var("CONMON_LOG_PLUGIN_PATH");
        }

        assert_eq!(got.as_deref(), Some(plugin.as_path()));
        Ok(())
    }

    // This test uses environment variables; serialize it with others to avoid cross-test races.
    #[test]
    #[serial]
    fn resolve_honors_multiple_colon_separated_entries_and_skips_empty() -> ConmonResult<()> {
        let d1 = tempdir()?;
        let d2 = tempdir()?;
        let d3 = tempdir()?;

        // Put plugin only in d2 to ensure order is respected.
        let name = "prio";
        let in_d2 = d2
            .path()
            .join(format!("lib{}_log_plugin.{}", name, DYLIB_EXT));
        fs::write(&in_d2, b"exists")?;

        // Form PATH like "<empty>:<d1>:<empty>:<d2>:<d3>"
        let path = format!(
            ":{}::{}:{}",
            d1.path().display(),
            d2.path().display(),
            d3.path().display()
        );
        unsafe {
            std::env::set_var("CONMON_LOG_PLUGIN_PATH", &path);
        }
        let got = resolve_plugin_path(name);
        unsafe {
            std::env::remove_var("CONMON_LOG_PLUGIN_PATH");
        }

        assert_eq!(got.as_deref(), Some(in_d2.as_path()));
        Ok(())
    }

    // This test uses environment variables; serialize it with others to avoid cross-test races.
    #[test]
    #[serial]
    fn resolve_returns_none_when_not_found_anywhere() -> ConmonResult<()> {
        // Ensure env var is not set to something helpful
        unsafe {
            std::env::remove_var("CONMON_LOG_PLUGIN_PATH");
        }
        let got = resolve_plugin_path("definitely-not-present");
        assert!(got.is_none());
        Ok(())
    }

    // This test uses environment variables; serialize it with others to avoid cross-test races.
    #[test]
    #[serial]
    fn load_returns_clear_error_when_not_found() -> ConmonResult<()> {
        // Guarantee that the name won't be found via env or defaults
        unsafe {
            std::env::remove_var("CONMON_LOG_PLUGIN_PATH");
        }
        let err = LoadedLogPlugin::load("missing_plugin_name", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Cannot load Log plugin"));
        assert!(msg.contains("not found"));
        Ok(())
    }

    // This test uses environment variables; serialize it with others to avoid cross-test races.
    #[test]
    #[serial]
    fn resolve_treats_name_with_separator_as_path_like() -> ConmonResult<()> {
        // If input contains a separator, it's treated as a path, not a name.
        // Put a file somewhere and pass that path.
        let d = tempdir()?;
        let p = d
            .path()
            .join(format!("librealname_log_plugin.{}", DYLIB_EXT));
        fs::write(&p, b"exists")?;
        let got = resolve_plugin_path(&p.to_string_lossy());
        assert_eq!(got.as_deref(), Some(p.as_path()));

        // If we pass a non-existing path (still path-like), it should return None early.
        let missing = d.path().join("nope/inside");
        assert!(resolve_plugin_path(&missing.to_string_lossy()).is_none());
        Ok(())
    }
}
