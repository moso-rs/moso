```rust title="src/routes/users.rs"
use moso::prelude::*;

/// A user, as the API accepts one.
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32)]
    pub username: String,
    /// Contact address.
    pub email: Email,
}

/// A user, as the API returns one.
#[derive(Schema)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
    /// Public handle.
    pub username: String,
}

/// Create a user.
#[endpoint]
async fn create(Json(body): Json<CreateUser>) -> Result<Created<UserOut>> {
    let id = 1;
    Ok(Created::at(
        format!("/users/{id}"),
        UserOut { id, username: body.username },
    ))
}

/// Everything this module serves.
pub fn router() -> Router {
    moso::routes! { POST "/users" => create }.tag("users")
}
```
