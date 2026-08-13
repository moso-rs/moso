//! Moso's hello world: one schema, one endpoint, one route table, one app.
//!
//! ```text
//! cargo run -p example-minimal
//! curl localhost:3000/hello/ada        # {"name":"ada","message":"Hello, ada!"}
//! open http://localhost:3000/docs      # the API reference, generated from this file
//! ```

use moso::AppBuilder;
use moso::prelude::*;

/// The body `GET /hello/{name}` returns.
#[derive(Schema)]
pub struct Greeting {
    /// Who was greeted.
    pub name: String,
    /// The greeting itself.
    pub message: String,
}

/// Greet someone by name.
#[endpoint]
async fn hello(
    Path(name): Path<String>,
    Inject(config): Inject<AppConfig>,
) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, {name}!", config.greeting),
        name,
    }))
}

/// Everything this application can be configured with.
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The word placed before the name. Override with `GREETING=Hei`.
    #[config(default = "Hello")]
    pub greeting: String,
}

/// The composition root: everything the application *is*, in one expression.
///
/// Returns the builder rather than a built [`App`] so that a test can override
/// a provider before `build()` runs the boot checks.
///
/// # Errors
/// Configuration that does not load.
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::load()?).mount(moso::routes! { GET "/hello/{name}" => hello }))
}
