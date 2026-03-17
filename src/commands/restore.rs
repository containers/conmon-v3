use crate::cli::RestoreCfg;
use crate::error::ConmonResult;
use crate::exit::OpenFilesSnapshot;
use crate::logging::plugin::LogPlugin;
use crate::runtime::args::RuntimeArgsGenerator;

pub struct Restore {
    cfg: RestoreCfg,
}

impl Restore {
    pub fn new(cfg: RestoreCfg) -> Self {
        Self { cfg }
    }

    pub fn exec(
        &self,
        log_plugin: &mut dyn LogPlugin,
        open_files: &OpenFilesSnapshot,
    ) -> ConmonResult<i32> {
        // Start the `runtime create` session.
        let mut runtime_session = crate::runtime::session::RuntimeSession::new(open_files.clone());
        runtime_session.launch(&self.cfg.common, self, false)?;

        // ===
        // Now, after the `launch()`, we are in the child process of our original process,
        // because we double-fork in the RuntimeProcess::spawn.
        // (See `RuntimeProcess::spawn` code and description for more information).
        // ===

        // In case of `--terminal`, wait until runtime creates the console socket.
        if self.cfg.common.terminal {
            runtime_session.wait_for_terminal_creation()?;
        }

        // Wait until the `runtime create` finishes and return an error in case it fails.
        runtime_session.wait_for_success(self.cfg.common.api_version, false)?;

        runtime_session.write_container_pid_file(&self.cfg.common)?;

        // ===
        // Now we wait for an external application like podman to really start the container.
        // and handle the containers stdio or its termination.
        // ===

        // Run the eventloop to forward log messages to log plugin.
        runtime_session.run_event_loop(
            log_plugin,
            self.cfg.common.leave_stdin_open,
            self.cfg.common.stdin,
        )?;

        // Wait for the `runtime exec` to finish and write its exit code.
        runtime_session.write_exit_code(self.cfg.common.api_version, false)?;

        Ok(runtime_session.container_exit_code())
    }
}

impl RuntimeArgsGenerator for Restore {
    fn add_global_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
        if self.cfg.systemd_cgroup {
            argv.push("--systemd-cgroup".into());
        }
        Ok(())
    }

    fn add_subcommand_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
        argv.extend([
            "restore".to_string(),
            "--bundle".to_string(),
            self.cfg.common.bundle.to_string_lossy().into_owned(),
            "--pid-file".to_string(),
            self.cfg
                .common
                .container_pidfile
                .to_string_lossy()
                .into_owned(),
            "--detach".to_string(),
            "--image-path".to_string(),
            self.cfg.restore_path.to_string_lossy().into_owned(),
            "--work-path".to_string(),
            self.cfg.common.bundle.to_string_lossy().into_owned(),
        ]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CommonCfg;
    use crate::runtime::args::generate_runtime_args;
    use std::path::PathBuf;

    fn mk_common(
        cid: &str,
        runtime_args: Vec<&str>,
        runtime_opts: Vec<&str>,
        no_pivot: bool,
        no_new_keyring: bool,
        pidfile: &str,
        bundle: &str,
    ) -> CommonCfg {
        CommonCfg {
            runtime: PathBuf::from("./runtime"),
            cid: cid.to_string(),
            runtime_args: runtime_args.into_iter().map(|s| s.to_string()).collect(),
            runtime_opts: runtime_opts.into_iter().map(|s| s.to_string()).collect(),
            no_pivot,
            no_new_keyring,
            container_pidfile: PathBuf::from(pidfile),
            bundle: PathBuf::from(bundle),
            ..Default::default()
        }
    }

    fn mk_restore_cfg(systemd_cgroup: bool, common: CommonCfg) -> RestoreCfg {
        RestoreCfg {
            systemd_cgroup,
            common,
            ..Default::default()
        }
    }

    #[test]
    fn generate_args_with_systemd_cgroup() {
        let common = mk_common(
            "cid123",
            vec!["--root", "/var/lib/runc"],
            vec!["--optA", "X"],
            false,
            false,
            "/tmp/pid-A",
            "/tmp/bundle-A",
        );
        let cfg = mk_restore_cfg(true, common);
        let restore = Restore::new(cfg);

        let argv = generate_runtime_args(&restore.cfg.common, &restore, None).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            "--systemd-cgroup".into(),
            "--root".into(),
            "/var/lib/runc".into(),
            "restore".into(),
            "--bundle".into(),
            "/tmp/bundle-A".into(),
            "--pid-file".into(),
            "/tmp/pid-A".into(),
            "--detach".into(),
            "--image-path".into(),
            "".into(),
            "--work-path".into(),
            "/tmp/bundle-A".into(),
            "--optA".into(),
            "X".into(),
            "cid123".into(),
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn generate_args_without_systemd_cgroup() {
        let common = mk_common(
            "cid456",
            vec![],
            vec!["--optB"],
            true,
            true,
            "/tmp/pid-B",
            "/tmp/bundle-B",
        );
        let cfg = mk_restore_cfg(false, common);
        let restore = Restore::new(cfg);

        let argv = generate_runtime_args(&restore.cfg.common, &restore, None).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            // (no --systemd-cgroup)
            // runtime_args empty
            "restore".into(),
            "--bundle".into(),
            "/tmp/bundle-B".into(),
            "--pid-file".into(),
            "/tmp/pid-B".into(),
            "--detach".into(),
            "--image-path".into(),
            "".into(),
            "--work-path".into(),
            "/tmp/bundle-B".into(),
            "--no-pivot".into(),
            "--no-new-keyring".into(),
            "--optB".into(),
            "cid456".into(),
        ];
        assert_eq!(argv, expected);
    }
}
