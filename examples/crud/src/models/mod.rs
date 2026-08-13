//! The application's types.
//!
//! One module per aggregate. Each one holds the domain type, the DTOs the API
//! speaks in, and the conversion between them — so a reader who wants to know
//! what a post *is* reads exactly one file.

pub mod post;

pub use post::{CreatePost, ListPosts, Post, PostOut, UpdatePost};
