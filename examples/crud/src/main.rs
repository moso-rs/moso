//! The whole binary. It never grows: everything the application *is* lives in
//! `lib.rs`, which is also what the tests drive.

#[tokio::main]
async fn main() -> moso::Result<()> {
    example_crud::app().await?.serve().await
}
