use crate::cli::CreateCfg;
use crate::error::{ConmonError, ConmonResult};
use crate::logging::plugin::LogPlugin;
use crate::runtime::args::{RuntimeArgsGenerator, generate_runtime_args};
use crate::runtime::run::{run_runtime, wait_for_runtime};
use crate::runtime::stdio::{create_pipe, handle_stdio};
use std::fs;
use std::process::Stdio;

pub struct Create {
    cfg: CreateCfg,
}

impl Create {
    pub fn new(cfg: CreateCfg) -> Self {
        Self { cfg }
    }

    // Helper function to read and return the container pid.
    fn read_container_pid(&self) -> ConmonResult<i32> {
        let contents = fs::read_to_string(self.cfg.container_pidfile.as_path())?;
        let pid = contents.trim().parse::<i32>().map_err(|e| {
            ConmonError::new(
                format!(
                    "Invalid PID contents in {}: {} ({})",
                    self.cfg.container_pidfile.display(),
                    contents.trim(),
                    e
                ),
                1,
            )
        })?;
        Ok(pid)
    }

    pub fn exec(&self, log_plugin: &mut dyn LogPlugin) -> ConmonResult<()> {
        // Generate the list of arguments for runtime.
        let runtime_args = generate_runtime_args(&self.cfg.common, self)?;

        // Generate pipes to handle stdio.
        let (mainfd_stdout, workerfd_stdout) = create_pipe()?;
        let (mainfd_stderr, workerfd_stderr) = create_pipe()?;

        // Run the `runtime create` and store our PID after first fork to `conmon_pidfile.
        let runtime_pid = run_runtime(
            &runtime_args,
            Stdio::null(), // TODO
            Stdio::from(workerfd_stdout),
            Stdio::from(workerfd_stderr),
        )?;
        if let Some(pidfile) = &self.cfg.conmon_pidfile {
            std::fs::write(pidfile, runtime_pid.to_string())?;
        }

        // ===
        // Now, after the `run_runtime`, we are in the child process of our original process
        // (See `run_runtime` code and description for more information).
        // ===

        // Wait until the `runtime create` finishes.
        let runtime_status = wait_for_runtime(runtime_pid)?;
        if runtime_status != 0 {
            return Err(ConmonError::new(
                format!("Runtime exited with status: {runtime_status}"),
                1,
            ));
        }

        // Read the container PID so we can wait for it later.
        let container_pid = self.read_container_pid()?;
        dbg!(container_pid);

        // ===
        // Now we wait for an external application like podman to really start the container.
        // and handle the containers stdio or its termination.
        // ===

        // Handle the stdio.
        handle_stdio(log_plugin, mainfd_stdout, mainfd_stderr)?;

        Ok(())
    }
}

impl RuntimeArgsGenerator for Create {
    fn add_global_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
        if self.cfg.systemd_cgroup {
            argv.push("--systemd-cgroup".into());
        }
        Ok(())
    }

    fn add_subcommand_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
        argv.extend([
            "create".to_string(),
            "--bundle".to_string(),
            self.cfg.bundle.to_string_lossy().into_owned(),
            "--pid-file".to_string(),
            self.cfg.container_pidfile.to_string_lossy().into_owned(),
        ]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CommonCfg;
    use std::path::PathBuf;

    fn mk_common(
        cid: &str,
        runtime_args: Vec<&str>,
        runtime_opts: Vec<&str>,
        no_pivot: bool,
        no_new_keyring: bool,
    ) -> CommonCfg {
        CommonCfg {
            runtime: PathBuf::from("./runtime"),
            cid: cid.to_string(),
            runtime_args: runtime_args.into_iter().map(|s| s.to_string()).collect(),
            runtime_opts: runtime_opts.into_iter().map(|s| s.to_string()).collect(),
            no_pivot,
            no_new_keyring,
            ..Default::default()
        }
    }

    fn mk_create_cfg(
        systemd_cgroup: bool,
        bundle: &str,
        pidfile: &str,
        common: CommonCfg,
    ) -> CreateCfg {
        CreateCfg {
            systemd_cgroup,
            bundle: PathBuf::from(bundle),
            container_pidfile: PathBuf::from(pidfile),
            common,
            ..Default::default()
        }
    }

    #[test]
    fn generate_args_create_with_systemd_cgroup() {
        let common = mk_common(
            "cid123",
            vec!["--root", "/var/lib/runc"],
            vec!["--optA", "X"],
            false,
            false,
        );
        let cfg = mk_create_cfg(true, "/tmp/bundle-A", "/tmp/pid-A", common);
        let create = Create::new(cfg);

        let argv =
            crate::runtime::args::generate_runtime_args(&create.cfg.common, &create).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            "--systemd-cgroup".into(),
            "--root".into(),
            "/var/lib/runc".into(),
            "create".into(),
            "--bundle".into(),
            "/tmp/bundle-A".into(),
            "--pid-file".into(),
            "/tmp/pid-A".into(),
            "--optA".into(),
            "X".into(),
            "cid123".into(),
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn generate_args_create_without_systemd_cgroup_with_generic_flags() {
        let common = mk_common("cid456", vec![], vec!["--optB"], true, true);
        let cfg = mk_create_cfg(false, "/tmp/bundle-B", "/tmp/pid-B", common);
        let create = Create::new(cfg);

        let argv =
            crate::runtime::args::generate_runtime_args(&create.cfg.common, &create).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            "create".into(),
            "--bundle".into(),
            "/tmp/bundle-B".into(),
            "--pid-file".into(),
            "/tmp/pid-B".into(),
            "--no-pivot".into(),
            "--no-new-keyring".into(),
            "--optB".into(),
            "cid456".into(),
        ];
        assert_eq!(argv, expected);
    }
}
