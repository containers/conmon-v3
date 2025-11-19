use std::process::ExitCode;

use crate::cli::CreateCfg;
use crate::error::ConmonResult;
use crate::logging::plugin::LogPlugin;
use crate::runtime::args::RuntimeArgsGenerator;

pub struct Create {
    cfg: CreateCfg,
}

impl Create {
    pub fn new(cfg: CreateCfg) -> Self {
        Self { cfg }
    }

    pub fn exec(&self, log_plugin: &mut dyn LogPlugin) -> ConmonResult<ExitCode> {
        // Start the `runtime create` session.
        let mut runtime_session = crate::runtime::session::RuntimeSession::new();
        runtime_session.launch(&self.cfg.common, self, false)?;

        // ===
        // Now, after the `launch()`, we are in the child process of our original process,
        // because we double-fork in the RuntimeProcess::spawn.
        // (See `RuntimeProcess::spawn` code and description for more information).
        // ===

        // Wait until the `runtime create` finishes and return an error in case it fails.
        runtime_session.wait_for_success(self.cfg.common.api_version)?;
        runtime_session.write_container_pid_file(&self.cfg.common)?;

        // ===
        // Now we wait for an external application like podman to really start the container.
        // and handle the containers stdio or its termination.
        // ===

        // Run the eventloop to forward log messages to log plugin.
        runtime_session.run_event_loop(log_plugin, self.cfg.common.leave_stdin_open)?;

        Ok(ExitCode::SUCCESS)
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
            self.cfg.common.bundle.to_string_lossy().into_owned(),
            "--pid-file".to_string(),
            self.cfg
                .common
                .container_pidfile
                .to_string_lossy()
                .into_owned(),
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

    fn mk_create_cfg(systemd_cgroup: bool, common: CommonCfg) -> CreateCfg {
        CreateCfg {
            systemd_cgroup,
            common,
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
            "/tmp/pid-A",
            "/tmp/bundle-A",
        );
        let cfg = mk_create_cfg(true, common);
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
        let common = mk_common(
            "cid456",
            vec![],
            vec!["--optB"],
            true,
            true,
            "/tmp/pid-B",
            "/tmp/bundle-B",
        );
        let cfg = mk_create_cfg(false, common);
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
