use std::process::ExitCode;

use crate::cli::ExecCfg;
use crate::error::ConmonResult;
use crate::logging::plugin::LogPlugin;
use crate::runtime::args::RuntimeArgsGenerator;

pub struct Exec {
    cfg: ExecCfg,
}

impl Exec {
    pub fn new(cfg: ExecCfg) -> Self {
        Self { cfg }
    }

    pub fn exec(&self, log_plugin: &mut dyn LogPlugin) -> ConmonResult<ExitCode> {
        let mut runtime_session = crate::runtime::session::RuntimeSession::new();
        runtime_session.launch(&self.cfg.common, self)?;

        // ===
        // Now, after the `launch`, we are in the child process of our original process
        // (See `RuntimeProcess::spawn` code and description for more information).
        // ===

        // Run the eventloop to forward log messages to log plugin.
        runtime_session.run_event_loop(log_plugin)?;

        // Wait for the `runtime exec` to finish and write its exit code.
        runtime_session.wait()?;
        runtime_session.write_container_pid_file(&self.cfg.common)?;
        runtime_session.write_exit_code(self.cfg.common.api_version)?;

        Ok(ExitCode::from(runtime_session.exit_code() as u8))
    }
}

impl RuntimeArgsGenerator for Exec {
    fn add_global_args(&self, _argv: &mut Vec<String>) -> ConmonResult<()> {
        Ok(())
    }

    fn add_subcommand_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
        argv.extend([
            "exec".to_string(),
            "--pid-file".to_string(),
            self.cfg
                .common
                .container_pidfile
                .to_string_lossy()
                .into_owned(),
            "--process".to_string(),
            self.cfg.exec_process_spec.to_string_lossy().into_owned(),
            "--detach".to_string(),
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
    ) -> CommonCfg {
        CommonCfg {
            runtime: PathBuf::from("./runtime"),
            cid: cid.to_string(),
            runtime_args: runtime_args.into_iter().map(|s| s.to_string()).collect(),
            runtime_opts: runtime_opts.into_iter().map(|s| s.to_string()).collect(),
            no_pivot,
            no_new_keyring,
            container_pidfile: PathBuf::from(pidfile),
            ..Default::default()
        }
    }

    fn mk_exec_cfg(proc_spec: &str, common: CommonCfg) -> ExecCfg {
        ExecCfg {
            exec_process_spec: PathBuf::from(proc_spec),
            common,
            ..Default::default()
        }
    }

    #[test]
    fn generate_args_exec_basic_ordering() {
        let common = mk_common(
            "cid123",
            vec!["--root", "/var/lib/runc"],
            vec!["--optA", "X"],
            false,
            false,
            "/tmp/pidfile",
        );
        let cfg = mk_exec_cfg("/tmp/process.json", common);
        let exec = Exec::new(cfg);

        let argv =
            crate::runtime::args::generate_runtime_args(&exec.cfg.common, &exec).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            "--root".into(),
            "/var/lib/runc".into(),
            "exec".into(),
            "--pid-file".into(),
            "/tmp/pidfile".into(),
            "--process".into(),
            "/tmp/process.json".into(),
            "--detach".into(),
            "--optA".into(),
            "X".into(),
            "cid123".into(),
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn generate_args_exec_with_generic_flags() {
        let common = mk_common("cid456", vec![], vec!["--optB"], true, true, "/run/pid");
        let cfg = mk_exec_cfg("/cfg/proc.json", common);
        let exec = Exec::new(cfg);

        let argv =
            crate::runtime::args::generate_runtime_args(&exec.cfg.common, &exec).expect("ok");

        let expected: Vec<String> = vec![
            "./runtime".into(),
            "exec".into(),
            "--pid-file".into(),
            "/run/pid".into(),
            "--process".into(),
            "/cfg/proc.json".into(),
            "--detach".into(),
            "--no-pivot".into(),
            "--no-new-keyring".into(),
            "--optB".into(),
            "cid456".into(),
        ];
        assert_eq!(argv, expected);
    }
}
