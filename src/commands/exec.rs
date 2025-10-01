use crate::error::ConmonResult;
use crate::cli::ExecCfg;

pub struct Exec {}

impl Exec {
    pub fn exec(&self, cfg: ExecCfg) -> ConmonResult<()> {
        println!("OK: exec");
        println!("  cid={}", cfg.common.cid);
        println!("  runtime={}", cfg.common.runtime.display());
        println!("  exec-process-spec={}", cfg.exec_process_spec.display());
        if cfg.attach { println!("  attach=true"); }

        Ok(())
    }
}
