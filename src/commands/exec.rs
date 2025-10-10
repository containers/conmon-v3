use crate::cli::ExecCfg;
use crate::error::ConmonResult;
use crate::runtime::args::{RuntimeArgsGenerator, generate_runtime_args};

pub struct Exec {
    cfg: ExecCfg,
}

impl Exec {
    pub fn new(cfg: ExecCfg) -> Self {
        Self { cfg }
    }

    pub fn exec(&self) -> ConmonResult<()> {
        let _runtime_args = generate_runtime_args(&self.cfg.common, self);

        Ok(())
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
            self.cfg.container_pidfile.to_string_lossy().into_owned(),
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

    fn mk_exec_cfg(pidfile: &str, proc_spec: &str, common: CommonCfg) -> ExecCfg {
        ExecCfg {
            container_pidfile: PathBuf::from(pidfile),
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
        );
        let cfg = mk_exec_cfg("/tmp/pidfile", "/tmp/process.json", common);
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
        let common = mk_common("cid456", vec![], vec!["--optB"], true, true);
        let cfg = mk_exec_cfg("/run/pid", "/cfg/proc.json", common);
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
