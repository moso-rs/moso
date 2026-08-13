//! What `App::build()` says when the application is wrong.
//!
//! The whole justification for the two-tier dependency model is that a missing
//! provider is a boot error and not a 500 at three in the morning. That claim is
//! only worth anything if the report is *readable*, so this file asserts the
//! rendered text, not just the fact that an error occurred.
//!
//! Line numbers are normalised before comparing: the report deliberately quotes
//! `file:line:column` for every route, and a snapshot that broke whenever a
//! comment moved would be deleted within a week.

#![allow(dead_code)]

use moso::prelude::*;
use moso::response::NoContent;

mod support;

// ---------------------------------------------------------------------------
// The pieces
// ---------------------------------------------------------------------------

/// A store the application forgets to provide.
#[derive(Debug, Default)]
pub struct Store;

/// A mailer the application also forgets to provide.
#[derive(Debug, Default)]
pub struct Mailer;

/// A clock the application *does* provide, so "did you mean" has something to
/// suggest and the report has to choose not to suggest it.
#[derive(Debug, Default)]
pub struct Clock;

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

/// Needs the store.
#[endpoint]
async fn list(Inject(_store): Inject<Store>) -> Result<NoContent> {
    Ok(NoContent)
}

/// Needs the store too, so one missing provider is reported once with two
/// routes under it rather than twice.
#[endpoint]
async fn show(Inject(_store): Inject<Store>) -> Result<NoContent> {
    Ok(NoContent)
}

/// Needs the mailer.
#[endpoint]
async fn invite(Inject(_mailer): Inject<Mailer>) -> Result<NoContent> {
    Ok(NoContent)
}

/// Needs nothing.
#[endpoint]
async fn ping() -> Result<NoContent> {
    Ok(NoContent)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Replace every `…rs:12:34` with `…rs:LINE` so the snapshot survives an edit.
fn normalise(report: &str) -> String {
    let mut out = String::with_capacity(report.len());
    let mut rest = report;
    while let Some(at) = rest.find(".rs:") {
        out.push_str(&rest[..at + 4]);
        rest = &rest[at + 4..];
        let digits = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || *c == ':')
            .map(|(index, c)| index + c.len_utf8())
            .last()
            .unwrap_or(0);
        rest = &rest[digits..];
        out.push_str("LINE");
    }
    out.push_str(rest);
    out
}

/// The plain-text boot report, or a panic when the application built.
fn report(builder: moso::AppBuilder) -> String {
    let error = builder
        .build()
        .map(|_| ())
        .expect_err("this application is broken");
    normalise(&error.to_string())
}

// ---------------------------------------------------------------------------
// Missing providers
// ---------------------------------------------------------------------------

#[test]
fn every_missing_provider_is_reported_in_one_pass() {
    let text = report(
        App::new(Cfg::default())
            .provide(Clock)
            .mount(moso::routes! {
                GET "/list"   => list,
                GET "/show"   => show,
                GET "/invite" => invite,
                GET "/ping"   => ping,
            }),
    );

    // Both problems, not just the first: a boot that stops at the first
    // mistake makes fixing five mistakes a five-round-trip exercise.
    assert!(
        text.starts_with("error: application failed to build (2 problems)"),
        "{text}"
    );
    assert!(
        text.contains("missing provider: `boot_report::Store`"),
        "{text}"
    );
    assert!(
        text.contains("missing provider: `boot_report::Mailer`"),
        "{text}"
    );

    // Every route that wanted the store, under one heading.
    assert!(text.contains("GET /list"), "{text}");
    assert!(text.contains("GET /show"), "{text}");

    // And a fix the reader can paste.
    assert!(
        text.contains("App::new(config).provide(value)"),
        "every problem must carry a mechanical fix:\n{text}"
    );
}

#[test]
fn the_missing_provider_report_reads_exactly_as_documented() {
    let text = report(App::new(Cfg::default()).mount(moso::routes! {
        GET "/list" => list,
    }));

    assert_eq!(
        text,
        "\
error: application failed to build (1 problem)

  x missing provider: `boot_report::Store`
      required by  GET /list  crates/moso/tests/boot_report.rs:LINE
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
",
        "the boot report is a deliverable; if this changed on purpose, update \
         the snapshot and docs/04-devex/41-diagnostics.md together\n\
         --- actual ---\n{text}"
    );
}

#[test]
fn a_provider_that_is_registered_is_not_reported() {
    App::new(Cfg::default())
        .provide(Store)
        .provide(Mailer)
        .mount(moso::routes! {
            GET "/list"   => list,
            GET "/invite" => invite,
        })
        .build()
        .expect("every provider is registered");
}

#[test]
fn a_near_miss_gets_a_did_you_mean_line() {
    /// The type the application meant to inject.
    #[derive(Debug, Default)]
    pub struct Mailerr;

    /// Injects the typo'd name.
    #[endpoint]
    async fn notify(Inject(_mailer): Inject<Mailerr>) -> Result<NoContent> {
        Ok(NoContent)
    }

    let text = report(
        App::new(Cfg::default())
            .provide(Mailer)
            .mount(moso::routes! { GET "/notify" => notify }),
    );

    assert!(text.contains("did you mean"), "{text}");
    assert!(text.contains("Mailer"), "{text}");
}

// ---------------------------------------------------------------------------
// Route conflicts
// ---------------------------------------------------------------------------

#[test]
fn two_registrations_of_the_same_route_are_a_conflict() {
    let text = report(
        App::new(Cfg::default()).mount(
            moso::routes! {
                GET "/ping" => ping,
            }
            .merge(moso::routes! {
                GET "/ping" => list,
            }),
        ),
    );

    assert!(
        text.contains("route conflict: GET /ping  and  GET /ping"),
        "{text}"
    );
    assert!(
        text.contains("the same method and path are registered twice"),
        "{text}"
    );
    assert!(
        text.contains("remove one registration"),
        "the fix has to be actionable:\n{text}"
    );
}

#[test]
fn two_parameter_names_at_the_same_position_are_a_conflict() {
    /// Reads `{id}`.
    #[endpoint]
    async fn by_id(Path(_id): Path<u64>) -> Result<NoContent> {
        Ok(NoContent)
    }

    /// Reads `{user_id}`.
    #[endpoint]
    async fn by_user_id(Path(_id): Path<u64>) -> Result<NoContent> {
        Ok(NoContent)
    }

    let text = report(App::new(Cfg::default()).mount(moso::routes! {
        GET "/users/{id}"      => by_id,
        GET "/users/{user_id}" => by_user_id,
    }));

    assert!(text.contains("route conflict"), "{text}");
    assert!(
        text.contains("path parameters must have the same name at the same position"),
        "matchit cannot tell these apart, and a 500 at runtime is the alternative:\n{text}"
    );
}

#[test]
fn a_conflict_is_reported_once_and_not_twice_as_a_duplicate_operation_id() {
    let text =
        report(App::new(Cfg::default()).mount(
            moso::routes! { GET "/ping" => ping }.merge(moso::routes! { GET "/ping" => list }),
        ));

    assert_eq!(
        text.matches("route conflict").count(),
        1,
        "one mistake, one problem:\n{text}"
    );
    assert!(
        !text.contains("duplicate operationId"),
        "the second registration is already dropped; reporting it twice sends the reader \
         chasing an error that disappears on its own:\n{text}"
    );
}

#[test]
fn a_route_that_shadows_the_probe_paths_is_refused() {
    /// Would sit on top of `/healthz`.
    #[endpoint]
    async fn health() -> Result<NoContent> {
        Ok(NoContent)
    }

    let text = report(App::new(Cfg::default()).mount(moso::routes! {
        GET "/healthz" => health,
    }));

    assert!(
        text.contains("/healthz"),
        "a route the outer router shadows must be a boot error, not a silent no-op:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Everything at once
// ---------------------------------------------------------------------------

#[test]
fn several_unrelated_problems_are_all_reported_and_ordered() {
    let text = report(
        App::new(Cfg::default()).mount(
            moso::routes! {
                GET "/list" => list,
                GET "/ping" => ping,
            }
            .merge(moso::routes! { GET "/ping" => show }),
        ),
    );

    assert!(
        text.starts_with("error: application failed to build (2 problems)"),
        "{text}"
    );

    let provider = text.find("missing provider").expect("a missing provider");
    let conflict = text.find("route conflict").expect("a route conflict");
    assert!(
        provider < conflict,
        "missing providers are usually the root cause and are reported first:\n{text}"
    );
}

#[test]
fn a_working_application_reports_nothing() {
    let builder = App::new(Cfg::default())
        .provide(Store)
        .mount(moso::routes! { GET "/list" => list });
    assert!(builder.errors().is_empty());
    builder.build().expect("nothing is wrong with it");
}
