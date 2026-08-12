//! Reaching the batteries the application was built with.
//!
//! Most of a test drives the application over HTTP and never mentions a battery
//! by name. Some tests need to look at the other side of a side effect: that a
//! row landed in the database, that a job was enqueued, that a welcome email was
//! *rendered and sent*. These accessors hand a test the very handle the
//! application under test resolved at boot — [`Db`](moso_orm::Db),
//! [`Kv`](moso_kv::Kv), [`Jobs`](moso_jobs::Jobs), a
//! [`Storage`](moso_storage::Storage) — so the assertion reads the same object
//! the handler wrote to, not a parallel one.
//!
//! # One accessor per battery, each behind its own feature
//!
//! | Accessor | Feature | Returns |
//! | --- | --- | --- |
//! | [`TestApp::db`] | `db` | `Arc<`[`Db`](moso_orm::Db)`>` |
//! | [`TestApp::kv`] | `kv` | [`Kv`](moso_kv::Kv) |
//! | [`TestApp::jobs`] | `jobs` | `Arc<`[`Jobs`](moso_jobs::Jobs)`>` |
//! | [`TestApp::storage`] | `storage` | `Arc<dyn `[`Storage`](moso_storage::Storage)`>` |
//! | [`TestApp::mail`] | `mail` | [`Mail`], a capturing-mailer assertion handle |
//!
//! Each feature pulls only its own battery crate, with the lean default backend
//! that needs no running service, and none of them is on by default — the
//! harness a test uses to check HTTP alone compiles none of this.
//!
//! # Why they fail loudly
//!
//! An accessor resolves a provider, and a provider the application never
//! registered is a mistake in the test, not a value to paper over. So a missing
//! handle **panics with the fix** rather than returning an `Option` a caller
//! would `unwrap` one line later with a worse message. This matches every other
//! assertion in the crate: the failure explains itself.
//!
//! # Mail is special
//!
//! A mailer is a `dyn Mailer` trait object, and "how many `WelcomeEmail`s were
//! sent" is not a question the trait answers — only the capturing backend
//! remembers. [`TestAppBuilder::capture_mail`] installs
//! [`MemoryMailer`](moso_mail::backend::MemoryMailer) as the application's
//! mailer and [`TestApp::mail`] hands back a [`Mail`] over it, so
//! `app.mail().assert_sent::<WelcomeEmail>(1)` reads what the handler actually
//! produced. Installing it is opt-in: the harness never silently replaces an
//! application's own mailer, because a test asserting on a real templated
//! message must get the real one.

#[cfg(feature = "mail")]
use std::sync::Arc;

use crate::TestApp;
#[cfg(feature = "mail")]
use crate::TestAppBuilder;

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

#[cfg(feature = "db")]
impl TestApp {
    /// The [`Db`](moso_orm::Db) the application resolved at boot.
    ///
    /// The same handle every handler holds, so a query run through it sees
    /// exactly what a handler wrote — including inside the same test database,
    /// when the application was pointed at a [`TestDb`](crate::TestDb).
    ///
    /// # Panics
    ///
    /// If the application registered no `Db` provider, with the line that wires
    /// one. Nothing here touches the database, so the panic is immediate.
    ///
    /// ```
    /// # use moso_test::prelude::*;
    /// # async fn after_a_write(app: &TestApp) {
    /// let db = app.db();
    /// // `db` is the application's own handle; query through it as usual.
    /// let _ = db;
    /// # }
    /// ```
    #[must_use]
    pub fn db(&self) -> std::sync::Arc<moso_orm::Db> {
        self.resolver().get::<moso_orm::Db>().unwrap_or_else(|_| {
            panic!(
                "{}",
                missing_battery(
                    "database",
                    "moso_orm::Db",
                    "point the application at a database and register the handle in \
                     your `app()` — `.provide(db)` — or build it against a `TestDb`",
                )
            )
        })
    }
}

// ---------------------------------------------------------------------------
// The key-value store
// ---------------------------------------------------------------------------

#[cfg(feature = "kv")]
impl TestApp {
    /// The [`Kv`](moso_kv::Kv) the application resolved at boot.
    ///
    /// `Kv` is a cheap handle over a shared backend, so the returned value reads
    /// and writes the same store the handlers use.
    ///
    /// # Panics
    ///
    /// If the application registered no `Kv` provider, with the line that wires
    /// one.
    ///
    /// ```
    /// # use moso_test::prelude::*;
    /// # async fn inspect_cache(app: &TestApp) {
    /// let kv = app.kv();
    /// // Read a key a handler was supposed to have set.
    /// let _ = kv;
    /// # }
    /// ```
    #[must_use]
    pub fn kv(&self) -> moso_kv::Kv {
        self.resolver()
            .get::<moso_kv::Kv>()
            .map(|handle| (*handle).clone())
            .unwrap_or_else(|_| {
                panic!(
                    "{}",
                    missing_battery(
                        "key-value store",
                        "moso_kv::Kv",
                        "register the cache in your `app()` — `.provide(kv)`",
                    )
                )
            })
    }
}

// ---------------------------------------------------------------------------
// The job queue
// ---------------------------------------------------------------------------

#[cfg(feature = "jobs")]
impl TestApp {
    /// The [`Jobs`](moso_jobs::Jobs) handle the application resolved at boot.
    ///
    /// The queue a handler enqueues onto, so a test can drain it, read its
    /// stats, or assert a job was scheduled — the same object, not a copy.
    ///
    /// # Panics
    ///
    /// If the application registered no `Jobs` provider, with the line that
    /// wires one.
    ///
    /// ```
    /// # use moso_test::prelude::*;
    /// # async fn after_enqueue(app: &TestApp) {
    /// let jobs = app.jobs();
    /// // Drain or inspect the queue the handler wrote to.
    /// let _ = jobs;
    /// # }
    /// ```
    #[must_use]
    pub fn jobs(&self) -> std::sync::Arc<moso_jobs::Jobs> {
        self.resolver()
            .get::<moso_jobs::Jobs>()
            .unwrap_or_else(|_| {
                panic!(
                    "{}",
                    missing_battery(
                        "job queue",
                        "moso_jobs::Jobs",
                        "register the queue in your `app()` — `.provide(jobs)`",
                    )
                )
            })
    }
}

// ---------------------------------------------------------------------------
// Object storage
// ---------------------------------------------------------------------------

#[cfg(feature = "storage")]
impl TestApp {
    /// The [`Storage`](moso_storage::Storage) the application resolved at boot.
    ///
    /// The trait object a handler stores bytes through, so a test can read an
    /// object back and assert on it.
    ///
    /// # Panics
    ///
    /// If the application registered no `dyn Storage` provider, with the line
    /// that wires one.
    ///
    /// ```
    /// # use moso_test::prelude::*;
    /// # async fn after_upload(app: &TestApp) {
    /// let storage = app.storage();
    /// // Fetch an object a handler was supposed to have written.
    /// let _ = storage;
    /// # }
    /// ```
    #[must_use]
    pub fn storage(&self) -> std::sync::Arc<dyn moso_storage::Storage> {
        self.resolver()
            .get_dyn::<dyn moso_storage::Storage>()
            .unwrap_or_else(|_| {
                panic!(
                    "{}",
                    missing_battery(
                        "object storage",
                        "dyn moso_storage::Storage",
                        "register the store in your `app()` — \
                         `.provide_dyn::<dyn Storage>(storage)`",
                    )
                )
            })
    }
}

/// The report a missing-battery accessor panics with.
#[cfg(any(
    feature = "db",
    feature = "kv",
    feature = "jobs",
    feature = "storage",
    feature = "mail"
))]
fn missing_battery(battery: &str, provider: &str, fix: &str) -> String {
    let mut out = crate::report::rule(&format!("moso-test: no {battery} handle"));
    out.push_str(&format!(
        "  the application under test registered no `{provider}` provider\n\n"
    ));
    out.push_str(&crate::report::section("fix", fix));
    out.push_str(&crate::report::rule_end());
    out
}

// ---------------------------------------------------------------------------
// Mail — the capturing handle
// ---------------------------------------------------------------------------

#[cfg(feature = "mail")]
impl TestAppBuilder {
    /// Install a capturing [`MemoryMailer`](moso_mail::backend::MemoryMailer) as
    /// the application's mailer, so [`TestApp::mail`] can assert on what was
    /// sent.
    ///
    /// The mailer is registered both as the `dyn Mailer` every handler resolves
    /// **and** as its concrete self, which is the copy the assertions read — one
    /// object, so a `.send()` in a handler is visible to
    /// `app.mail().assert_sent::<T>(..)` immediately after.
    ///
    /// This replaces whatever mailer the application's own `app()` wired, and
    /// only when asked: a test that wants to exercise a real templated backend
    /// simply does not call this.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso_test::prelude::*;
    /// use moso_mail::{Address, Email, Mailer, Result as MailResult};
    /// use std::sync::Arc;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Service name.
    /// #     #[config(default = "users")] pub name: String }
    ///
    /// /// The message a new account receives.
    /// pub struct Welcome {
    ///     /// Who signed up.
    ///     pub to: Address,
    /// }
    /// impl Email for Welcome {
    ///     fn to(&self) -> Vec<Address> { vec![self.to.clone()] }
    ///     fn subject(&self) -> MailResult<String> { Ok("Welcome".to_owned()) }
    ///     fn html(&self) -> MailResult<String> { Ok("<b>hi</b>".to_owned()) }
    ///     fn text(&self) -> MailResult<String> { Ok("hi".to_owned()) }
    /// }
    ///
    /// /// Sign up, and send the welcome mail.
    /// #[moso::endpoint]
    /// async fn signup(Inject(mailer): Inject<dyn Mailer>)
    ///     -> Result<moso::response::NoContent>
    /// {
    ///     mailer.send(&Welcome { to: Address::new("ada@example.com")? }).await?;
    ///     Ok(moso::response::NoContent)
    /// }
    ///
    /// /// The composition root every Moso application exposes.
    /// fn app() -> moso::AppBuilder {
    ///     moso::App::new(AppConfig { name: "users".to_owned() })
    ///         .mount(moso::routes! { POST "/signup" => signup })
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso::Result<()> {
    /// let app = TestApp::builder().app(app()).capture_mail().spawn().await?;
    ///
    /// app.client().post("/signup").send().await.assert_status(204);
    /// app.mail().assert_sent::<Welcome>(1);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn capture_mail(self) -> Self {
        self.customise(|app| {
            let mailer = Arc::new(moso_mail::backend::MemoryMailer::new());
            let as_trait: Arc<dyn moso_mail::Mailer> = mailer.clone();
            app.provide_dyn::<dyn moso_mail::Mailer>(as_trait)
                .provide_arc(mailer)
        })
    }
}

#[cfg(feature = "mail")]
impl TestApp {
    /// The capturing mailer, as a handle that can assert on what was sent.
    ///
    /// Requires [`TestAppBuilder::capture_mail`] on the builder — that is what
    /// installs the [`MemoryMailer`](moso_mail::backend::MemoryMailer) this
    /// reads.
    ///
    /// # Panics
    ///
    /// If no capturing mailer was installed, with the one-line fix. A `dyn
    /// Mailer` an application wired itself is not enough: only the memory backend
    /// remembers messages, so the assertions need it specifically.
    ///
    /// ```
    /// # use moso_test::prelude::*;
    /// # async fn check(app: &TestApp) {
    /// // Everything the memory backend recorded is one call away.
    /// let messages = app.mail().sent();
    /// # let _ = messages;
    /// # }
    /// ```
    #[must_use]
    pub fn mail(&self) -> Mail {
        let mailer = self
            .resolver()
            .get::<moso_mail::backend::MemoryMailer>()
            .unwrap_or_else(|_| {
                panic!(
                    "{}",
                    missing_battery(
                        "capturing mailer",
                        "moso_mail::backend::MemoryMailer",
                        "call `.capture_mail()` on the builder before `spawn()`",
                    )
                )
            });
        Mail { mailer }
    }
}

/// Assertions over the mail a [`TestApp`] captured.
///
/// Returned by [`TestApp::mail`]. Every assertion returns `&Self`, so they
/// chain, and reads what the application's handlers actually sent through the
/// capturing [`MemoryMailer`](moso_mail::backend::MemoryMailer).
#[cfg(feature = "mail")]
#[derive(Clone)]
pub struct Mail {
    mailer: Arc<moso_mail::backend::MemoryMailer>,
}

#[cfg(feature = "mail")]
impl Mail {
    /// Assert that exactly `expected` messages of type `T` were sent.
    ///
    /// Matches by the [`Email`](moso_mail::Email)'s Rust type, the same way
    /// [`MemoryMailer::count_of`](moso_mail::backend::MemoryMailer::count_of)
    /// does.
    ///
    /// # Panics
    ///
    /// If the count differs, printing what *was* sent, by kind.
    pub fn assert_sent<T: moso_mail::Email + ?Sized>(&self, expected: usize) -> &Self {
        let actual = self.mailer.count_of::<T>();
        if actual != expected {
            panic!(
                "{}",
                self.report(&format!(
                    "expected {expected} {} message(s) to have been sent, {actual} were",
                    last_segment(std::any::type_name::<T>()),
                ))
            );
        }
        self
    }

    /// Assert that no mail at all was sent.
    ///
    /// # Panics
    ///
    /// If anything was sent, listing it by kind.
    pub fn assert_none_sent(&self) -> &Self {
        let sent = self.mailer.sent_count();
        if sent != 0 {
            panic!(
                "{}",
                self.report(&format!(
                    "expected no mail to have been sent, {sent} message(s) were"
                ))
            );
        }
        self
    }

    /// How many messages of type `T` were captured.
    #[must_use]
    pub fn count<T: moso_mail::Email + ?Sized>(&self) -> usize {
        self.mailer.count_of::<T>()
    }

    /// Every message of type `T`, for an assertion the count cannot express.
    #[must_use]
    pub fn sent_of<T: moso_mail::Email + ?Sized>(&self) -> Vec<moso_mail::RenderedEmail> {
        self.mailer.sent_of::<T>()
    }

    /// Every message captured so far, newest last.
    #[must_use]
    pub fn sent(&self) -> Vec<moso_mail::RenderedEmail> {
        self.mailer.sent()
    }

    /// The mailer as the `dyn Mailer` trait object handlers see.
    #[must_use]
    pub fn mailer(&self) -> Arc<dyn moso_mail::Mailer> {
        self.mailer.clone()
    }

    /// Forget everything captured so far.
    pub fn clear(&self) {
        self.mailer.clear();
    }

    /// A failure report headed `headline`, listing what was captured by kind.
    fn report(&self, headline: &str) -> String {
        let mut out = crate::report::rule("moso-test: mail assertion failed");
        out.push_str(&format!("  {headline}\n\n"));

        let sent = self.mailer.sent();
        let body = if sent.is_empty() {
            "(nothing was sent)".to_owned()
        } else {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for message in &sent {
                let kind = last_segment(&message.kind).to_owned();
                match counts.iter_mut().find(|(name, _)| *name == kind) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((kind, 1)),
                }
            }
            counts
                .into_iter()
                .map(|(kind, count)| format!("{count}  {kind}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        out.push_str(&crate::report::section("sent", &body));
        out.push_str(&crate::report::rule_end());
        out
    }
}

#[cfg(feature = "mail")]
impl core::fmt::Debug for Mail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mail")
            .field("sent", &self.mailer.sent_count())
            .finish_non_exhaustive()
    }
}

/// The last `::`-separated segment of a Rust type name.
///
/// `alloc::string::String` becomes `String`, so a report names a message type
/// the way its author wrote it rather than by its whole module path.
#[cfg(feature = "mail")]
fn last_segment(type_name: &str) -> &str {
    // Generic arguments can contain `::` of their own; the message kinds this is
    // used on are plain paths, so splitting on the last `::` before any `<` is
    // enough and keeps a generic tail intact.
    let head = type_name.split('<').next().unwrap_or(type_name);
    match head.rfind("::") {
        Some(index) => &type_name[index + 2..],
        None => type_name,
    }
}

#[cfg(all(test, feature = "mail"))]
mod tests {
    use super::last_segment;

    #[test]
    fn the_last_segment_is_the_bare_type_name() {
        assert_eq!(last_segment("alloc::string::String"), "String");
        assert_eq!(last_segment("Welcome"), "Welcome");
        assert_eq!(last_segment("my_app::mail::Welcome"), "Welcome");
    }
}
