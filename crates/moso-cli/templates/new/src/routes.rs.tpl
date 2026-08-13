//! The HTTP surface.
//!
//! One type definition per payload drives four things at once: deserialisation,
//! validation, the OpenAPI schema, and the JSON Pointer in the 422 a client
//! gets when it sends something wrong. There is no second place to keep in sync.

use moso::prelude::*;

/// What `POST /greetings` accepts.
#[derive(Schema, Debug, Clone)]
pub struct NewGreeting {
    /// Who to greet.
    ///
    /// `len` is one attribute and it produces two things: the runtime check
    /// that rejects an empty name with `422` and `/name`, and the
    /// `minLength`/`maxLength` in the published schema. They cannot disagree.
    #[schema(len = 1..=64)]
    pub name: String,
}

/// What the greeting endpoints return.
#[derive(Schema, Debug, Clone)]
pub struct Greeting {
    /// The rendered message.
    pub message: String,
}

/// Greet the world.
///
/// The first line of this doc comment becomes the operation's `summary`, and
/// the rest becomes its `description`. Writing the documentation *is* writing
/// the code.
#[endpoint]
async fn hello(Inject(config): Inject<crate::AppConfig>) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, world", config.greeting),
    }))
}

/// Greet someone by name.
///
/// `Json<NewGreeting>` cannot exist unless the body parsed *and* validated, so
/// this function never has to check its own input.
#[endpoint]
async fn greet(
    Inject(config): Inject<crate::AppConfig>,
    Json(body): Json<NewGreeting>,
) -> Result<Created<Greeting>> {
    Ok(Created::at(
        "/greetings",
        Greeting {
            message: format!("{}, {}", config.greeting, body.name),
        },
    ))
}

/// Every route this application serves.
///
/// `routes!` is the registration form to reach for: the method and the path sit
/// next to the handler, and the whole table is one thing to read. It rewrites
/// each name to the operation type `#[endpoint]` generated, which is how the
/// documentation travels with the handler instead of being repeated here.
pub fn router() -> Router {
    moso::routes! {
        GET  "/"          => hello,
        POST "/greetings" => greet,
    }
    .tag("greetings")
}
