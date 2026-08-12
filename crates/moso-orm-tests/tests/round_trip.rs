//! Derived entities against a real server: write, read back, preload, and the
//! two error shapes an HTTP layer has to render.
//!
//! SQLite runs everywhere. PostgreSQL gates on `DATABASE_URL` and skips with a
//! message naming the command that starts one, so this file is green on a
//! machine with no Docker — and *runs* on the one in CI.

use moso::db::insert::upsert::status_code;
use moso::db::prelude::*;
use moso::db::{ConstraintKind, Preload, StatementMark};
use moso::{Entity, Projection};

// ───────────────────────────────────────────────────────────────────────────
// Fixture
// ───────────────────────────────────────────────────────────────────────────

/// Someone who writes posts.
#[derive(Entity, Debug, Clone)]
#[entity(table = "rt_derive_authors")]
pub struct Author {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// Login identity; one row per address.
    #[entity(unique)]
    pub email: String,
    /// Everything this author wrote.
    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}

/// One post, by one author.
#[derive(Entity, Debug, Clone)]
#[entity(table = "rt_derive_posts")]
pub struct Post {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,
    /// The headline.
    pub title: String,
    /// How many people read it.
    pub views: i64,
    /// Whose post this is.
    pub author_id: i64,
    /// Who wrote it.
    #[entity(belongs_to = Author, fk = "author_id")]
    pub author: Related<Author>,
}

/// The two columns a listing page needs.
#[derive(Projection, Debug, Clone, PartialEq, Eq)]
#[projection(entity = Post)]
pub struct PostListing {
    /// The primary key.
    pub id: i64,
    /// The headline.
    pub title: String,
}

/// How many authors the fixture has.
const AUTHORS: i64 = 20;
/// How many posts each author has.
const POSTS_EACH: i64 = 5;

// ───────────────────────────────────────────────────────────────────────────
// Fixture management
// ───────────────────────────────────────────────────────────────────────────

/// Drops and recreates the two tables, then fills them.
///
/// Written with `RawQuery` rather than `moso-migrate` on purpose: this file is
/// testing the *derive* and the executor, and a failure here should not be
/// ambiguous between them and the migration generator.
async fn seed(db: &Db, autoincrement: &str) -> Result<()> {
    for statement in [
        "DROP TABLE IF EXISTS rt_derive_posts",
        "DROP TABLE IF EXISTS rt_derive_authors",
    ] {
        RawQuery::new(statement).execute(db).await?;
    }
    RawQuery::new(format!(
        "CREATE TABLE rt_derive_authors (id {autoincrement} PRIMARY KEY, \
         email text NOT NULL UNIQUE)"
    ))
    .execute(db)
    .await?;
    RawQuery::new(format!(
        "CREATE TABLE rt_derive_posts (id {autoincrement} PRIMARY KEY, \
         title text NOT NULL, views bigint NOT NULL, author_id bigint NOT NULL \
         REFERENCES rt_derive_authors(id))"
    ))
    .execute(db)
    .await?;

    for author in 1..=AUTHORS {
        Author::insert(NewAuthor {
            id: author,
            email: format!("author{author}@example.test"),
        })
        .execute(db)
        .await?;
        for post in 1..=POSTS_EACH {
            Post::insert(NewPost {
                id: (author - 1) * POSTS_EACH + post,
                title: format!("post {post} by {author}"),
                views: post * 10,
                author_id: author,
            })
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// Removes the fixture, so a shared server does not accumulate tables.
async fn drop_fixture(db: &Db) {
    for statement in [
        "DROP TABLE IF EXISTS rt_derive_posts",
        "DROP TABLE IF EXISTS rt_derive_authors",
    ] {
        let _ = RawQuery::new(statement).execute(db).await;
    }
}

/// A SQLite handle on a file of this process's own.
///
/// A file rather than `:memory:`, because every connection in a pool gets its
/// own in-memory database and the fixture would vanish between statements.
async fn sqlite(tag: &str) -> (Db, std::path::PathBuf) {
    let path =
        std::env::temp_dir().join(format!("moso-derive-{tag}-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("an on-disk SQLite database always opens");
    (db, path)
}

/// A PostgreSQL handle, or `None` with a message that says how to get one.
async fn postgres() -> Option<Db> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping the PostgreSQL leg: DATABASE_URL is not set.\n  \
             help: docker compose -f compose.test.yaml up -d, then \
             DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test cargo test"
        );
        return None;
    };
    Some(
        Db::connect_url(&url)
            .await
            .expect("DATABASE_URL names a reachable server"),
    )
}

// ───────────────────────────────────────────────────────────────────────────
// The criteria, once, against whichever server is handed in
// ───────────────────────────────────────────────────────────────────────────

/// Every criterion this file asserts, so the two backends run the same code.
async fn criteria(db: &Db) -> Result<()> {
    // The write path and the read path agree: what `#[derive(Entity)]` wrote
    // is what its `from_row` reads back.
    let one = Post::find(3).fetch_one(db).await?;
    assert_eq!(one.id, 3);
    assert_eq!(one.title, "post 3 by 1");
    assert_eq!(one.views, 30);
    assert!(
        one.author.is_not_loaded(),
        "N2: a plain fetch leaves the relation unloaded"
    );

    // N3 — the whole table plus one relation is two statements, whatever the
    // row count. This is the assertion, not an inspection of the SQL.
    let mark: StatementMark = db.statements().mark();
    let posts = Post::query().with(Post::AUTHOR).fetch_all(db).await?;
    assert_eq!(posts.len() as i64, AUTHORS * POSTS_EACH);
    assert_eq!(
        db.statements().since(mark),
        2,
        "N3: one statement for the posts, one for every author they point at"
    );
    assert!(
        posts.iter().all(|post| post.author().is_ok()),
        "every row's relation is loaded, from the one extra statement"
    );

    // N3, nested: +1 per level, still not per row.
    let mark = db.statements().mark();
    let authors = Author::query()
        .with(Preload::from(Author::POSTS).with(Post::AUTHOR))
        .fetch_all(db)
        .await?;
    assert_eq!(authors.len() as i64, AUTHORS);
    assert_eq!(
        db.statements().since(mark),
        3,
        "N3: authors, their posts, and the posts' authors — three statements"
    );

    // N4 — a filter that is not there costs nothing, and one that is applies.
    let none: Option<i64> = None;
    let all = Post::query()
        .filter_opt(none.map(|v| Post::VIEWS.gt(v)))
        .count(db)
        .await?;
    assert_eq!(all as i64, AUTHORS * POSTS_EACH);
    let some = Post::query()
        .filter_opt(Some(Post::VIEWS.gt(30)))
        .count(db)
        .await?;
    assert_eq!(some as i64, AUTHORS * 2, "views 40 and 50 for each author");

    // N5 — a derived projection reads its own columns, decoded into its own
    // struct.
    let listing: Vec<PostListing> = Post::query()
        .filter(Post::ID.eq(3))
        .project::<PostListing>()
        .fetch_all(db)
        .await?;
    assert_eq!(
        listing,
        vec![PostListing {
            id: 3,
            title: "post 3 by 1".to_owned(),
        }]
    );

    // N8 — the raw-SQL escape hatch, through the `sql!` macro, decoding into a
    // derived projection.
    let threshold = 40_i64;
    let raw: Vec<PostListing> =
        moso::sql!("select id, title from rt_derive_posts where views >= {threshold} order by id")
            .project_all::<PostListing>(db)
            .await?;
    assert_eq!(raw.len() as i64, AUTHORS * 2);
    assert_eq!(raw[0].id, 4);

    Ok(())
}

/// N7 — a unique violation names the column a client can fix.
async fn unique_violation_is_a_409_with_a_pointer(db: &Db) -> Result<()> {
    let error = Author::insert(NewAuthor {
        id: AUTHORS + 1,
        email: "author1@example.test".to_owned(),
    })
    .execute(db)
    .await
    .expect_err("that address is already taken");

    assert_eq!(status_code(&error), 409, "{error}");
    assert_eq!(
        error.field_pointer().as_deref(),
        Some("/email"),
        "the problem document has to point at the field: {error}"
    );
    match &error {
        Error::UniqueViolation(violation) => {
            assert_eq!(violation.kind(), ConstraintKind::Unique);
            assert_eq!(violation.entity(), "Author");
        }
        other => panic!("expected a unique violation, got {other}"),
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// The two legs
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_derived_entities_round_trip_on_sqlite() {
    let (db, path) = sqlite("round-trip").await;
    let outcome = async {
        seed(&db, "bigint").await?;
        criteria(&db).await?;
        unique_violation_is_a_409_with_a_pointer(&db).await
    }
    .await;
    drop_fixture(&db).await;
    db.close().await;
    let _ = std::fs::remove_file(&path);
    outcome.expect("every criterion holds on SQLite");
}

#[tokio::test]
async fn the_derived_entities_round_trip_on_postgres() {
    let Some(db) = postgres().await else { return };
    let outcome = async {
        seed(&db, "bigint").await?;
        criteria(&db).await?;
        unique_violation_is_a_409_with_a_pointer(&db).await
    }
    .await;
    drop_fixture(&db).await;
    db.close().await;
    outcome.expect("every criterion holds on PostgreSQL");
}

/// N8 — the pool itself, which is the other half of the escape hatch.
#[tokio::test]
async fn the_raw_pool_is_reachable() {
    let (db, path) = sqlite("pool").await;
    assert!(
        db.sqlite_pool().is_some(),
        "N8: `Db::sqlite_pool()` hands the caller sqlx's own pool"
    );
    assert!(db.postgres_pool().is_none(), "and not the other one");
    db.close().await;
    let _ = std::fs::remove_file(&path);

    let Some(db) = postgres().await else { return };
    assert!(db.postgres_pool().is_some());
    assert!(db.sqlite_pool().is_none());
    db.close().await;
}
