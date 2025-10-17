use crate::cli::CreateCfg;
use crate::error::ConmonResult;
use crate::runtime::args::{RuntimeArgsGenerator, generate_runtime_args};

pub struct Create {
    cfg: CreateCfg,
}

impl Create {
    pub fn new(cfg: CreateCfg) -> Self {
        Self { cfg }
    }

    pub fn exec(&self) -> ConmonResult<()> {
        let _runtime_args = generate_runtime_args(&self.cfg.common, self);

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
