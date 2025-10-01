use crate::error::ConmonResult;
use crate::cli::RestoreCfg;

pub struct Restore {}

impl Restore {
    pub fn exec(&self, cfg: RestoreCfg) -> ConmonResult<()> {
        println!("OK: restore");
        println!("  cid={}", cfg.common.cid);
        println!("  runtime={}", cfg.common.runtime.display());
        println!("  restore={}", cfg.restore_path.display());

        Ok(())
    }
}
