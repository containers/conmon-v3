use crate::cli::CommonCfg;
use crate::error::ConmonResult;

/// Trait for constructing the runtime argv.
/// Implementors append global/common flags and then subcommand-specificf,
/// flags to `argv`, returning an error on failure.
pub trait RuntimeArgsGenerator {
    /// Append arguments common to all invocations (e.g., global runtime flags) to `argv`.
    fn add_global_args(&self, argv: &mut Vec<String>) -> ConmonResult<()>;
    /// Append arguments specific to the particular subcommand (e.g., exec/create/restore) to `argv`.
    fn add_subcommand_args(&self, argv: &mut Vec<String>) -> ConmonResult<()>;
}

/// Generates the runtime binary arguments from the `Commoncfg`.
/// The `args_gen` functions are used to generate subcommand specific
/// arguments.
pub fn generate_runtime_args(
    o: &CommonCfg,
    args_gen: &impl RuntimeArgsGenerator,
) -> ConmonResult<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();

    // runtime path (binary) first
    argv.push(o.runtime.to_string_lossy().into_owned());

    // Argument specific global args.
    args_gen.add_global_args(&mut argv)?;

    // Extra runtime args (appear right after the runtime path / global flags)
    argv.extend(o.runtime_args.iter().map(|s| s.to_string()));

    // Argument specific subcommand args.
    args_gen.add_subcommand_args(&mut argv)?;

    // Generic subcommand args.
    if o.no_pivot {
        argv.push("--no-pivot".into());
    }
    if o.no_new_keyring {
        argv.push("--no-new-keyring".into());
    }

    // Generic passthrough runtime opts (after subcommand-specific flags)
    argv.extend(o.runtime_opts.iter().map(|s| s.to_string()));

    // TODO: Optional console socket
    // if let Some(cs) = csname {
    //     argv.extend(["--console-socket".to_string(), cs.to_string()]);
    // }

    // Container ID last
    argv.push(o.cid.to_string());
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ConmonError, ConmonResult};

    struct OkGen {
        globals: Vec<String>,
        subs: Vec<String>,
    }
    impl RuntimeArgsGenerator for OkGen {
        fn add_global_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
            argv.extend(self.globals.iter().cloned());
            Ok(())
        }
        fn add_subcommand_args(&self, argv: &mut Vec<String>) -> ConmonResult<()> {
            argv.extend(self.subs.iter().cloned());
            Ok(())
        }
    }

    struct FailGlobal;
    impl RuntimeArgsGenerator for FailGlobal {
        fn add_global_args(&self, _argv: &mut Vec<String>) -> ConmonResult<()> {
            Err(ConmonError::new("global failure", 1))
        }
        fn add_subcommand_args(&self, _argv: &mut Vec<String>) -> ConmonResult<()> {
            unreachable!("should not be called on global failure")
        }
    }

    struct FailSub;
    impl RuntimeArgsGenerator for FailSub {
        fn add_global_args(&self, _argv: &mut Vec<String>) -> ConmonResult<()> {
            Ok(())
        }
        fn add_subcommand_args(&self, _argv: &mut Vec<String>) -> ConmonResult<()> {
            Err(ConmonError::new("subcommand failure", 1))
        }
    }

    #[test]
    fn orders_runtime_args_correctly() {
        let common = CommonCfg {
            runtime: "./runtime".into(),
            cid: "abc123".into(),
            runtime_args: vec!["--root".into(), "/var/lib/runc".into()],
            runtime_opts: vec!["--optA".into(), "x".into()],
            no_pivot: true,
            no_new_keyring: true,
            ..Default::default()
        };

        let args_gen = OkGen {
            globals: vec!["--debug".into()],
            subs: vec!["create".into(), "--bundle".into(), "/bundle".into()],
        };

        let argv = generate_runtime_args(&common, &args_gen).expect("ok");

        let expected = vec![
            "./runtime",
            "--debug",
            "--root",
            "/var/lib/runc",
            "create",
            "--bundle",
            "/bundle",
            "--no-pivot",
            "--no-new-keyring",
            "--optA",
            "x",
            "abc123",
        ];
        assert_eq!(argv, expected);
    }

    #[test]
    fn propagates_error_from_add_global_args() {
        let common = CommonCfg {
            runtime: "./runtime".into(),
            cid: "cid".into(),
            runtime_args: vec![],
            runtime_opts: vec![],
            no_pivot: false,
            no_new_keyring: false,
            ..Default::default()
        };

        let err = generate_runtime_args(&common, &FailGlobal).unwrap_err();
        assert!(err.to_string().contains("global failure"));
    }

    #[test]
    fn propagates_error_from_add_subcommand_args() {
        let common = CommonCfg {
            runtime: "./runtime".into(),
            cid: "cid".into(),
            runtime_args: vec!["--ra".into()],
            runtime_opts: vec!["--ro".into()],
            no_pivot: false,
            no_new_keyring: false,
            ..Default::default()
        };

        let err = generate_runtime_args(&common, &FailSub).unwrap_err();
        assert!(err.to_string().contains("subcommand failure"));
    }
}
