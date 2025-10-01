use crate::error::ConmonResult;
use crate::cli::RunCfg;

pub struct Run {}

impl Run {
    pub fn exec(&self, cfg: RunCfg) -> ConmonResult<()> {
        println!("OK: run");
        println!("  cid={}", cfg.common.cid);
        println!("  runtime={}", cfg.common.runtime.display());
        if let Some(b) = cfg.bundle { println!("  bundle={}", b.display()); }
        println!("  container-pidfile={}", cfg.container_pidfile.display());

        Ok(())
    }
}
