//! The entry point.
//!
//! Deliberately tiny. Everything it needs is a public function in the library
//! next door, which is what lets `tests/api.rs` boot the identical application.

use @@LIB_NAME@@::{build, dump};

#[tokio::main]
async fn main() -> moso::Result<()> {
    // `moso routes`, `moso openapi export` and `moso config` run this binary
    // with a `--dump-*` flag and read one document off stdout. See `src/dump.rs`
    // for the full protocol — it is twenty lines and it is yours to change.
    if let Some(requested) = dump::requested() {
        return dump::run(requested, &build()?).await;
    }
@@DB_DISPATCH@@
    // Binds the address from configuration, installs signal handlers, runs the
    // startup hooks, serves, and drains in-flight requests on SIGTERM.
    build()?.serve().await
}
