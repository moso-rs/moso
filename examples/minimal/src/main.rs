//! The binary is a shim over the library, so tests boot the real application
//! rather than a parallel copy of it. This file never grows.

#[tokio::main]
async fn main() -> moso::Result<()> {
    let app = example_minimal::app()?.build()?;
    println!("docs on http://{}/docs", app.state().server().bind);
    app.serve().await
}
