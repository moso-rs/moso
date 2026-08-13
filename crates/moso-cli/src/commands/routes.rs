//! `moso routes` — the route table, in the shape `40-cli.md` prints it.
//!
//! ```text
//! METHOD  PATH                HANDLER        AUTH      TAGS    SOURCE
//! GET     /api/v1/users       users::list    session   users   src/routes/users.rs:14
//! ```
//!
//! The rows come from the application itself (`--dump-routes`), not from
//! parsing source, so a route registered by a loop, a `nest`, or a function in
//! a dependency shows up exactly as it will be served.

use serde_json::Value;

use crate::cli::RoutesArgs;
use crate::exit::{CliError, Outcome};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// One registered route, as the application described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The HTTP method.
    pub method: String,
    /// The full path, prefixes applied.
    pub path: String,
    /// The handler's name.
    pub handler: String,
    /// The security schemes required. Empty means unauthenticated.
    pub security: Vec<String>,
    /// The OpenAPI tags.
    pub tags: Vec<String>,
    /// Where `#[endpoint]` was written, when the handler carries a location.
    pub source: Option<String>,
    /// Whether the route carries an `#[endpoint]` description.
    pub documented: bool,
    /// Whether it is excluded from the OpenAPI document.
    pub hidden: bool,
    /// Whether clients should migrate away from it.
    pub deprecated: bool,
}

impl Route {
    /// Read one row out of the JSON the application printed.
    fn from_json(value: &Value) -> Self {
        Self {
            method: string(value, "method"),
            path: string(value, "path"),
            handler: string(value, "handler"),
            security: strings(value, "security"),
            tags: strings(value, "tags"),
            source: value
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_owned),
            documented: flag(value, "documented"),
            hidden: flag(value, "hidden"),
            deprecated: flag(value, "deprecated"),
        }
    }

    /// The `METHOD` column.
    ///
    /// Upper-cased here rather than at the source: the wire form of a method is
    /// upper case, the OpenAPI key for one is lower case, and the dump carries
    /// the OpenAPI spelling. A route table is read as HTTP, not as OpenAPI.
    fn method(&self) -> String {
        self.method.to_uppercase()
    }

    /// The `AUTH` column: the schemes, or `-` for a public route.
    fn auth(&self) -> String {
        if self.security.is_empty() {
            "-".to_owned()
        } else {
            self.security.join(",")
        }
    }

    /// The `TAGS` column.
    fn tags(&self) -> String {
        if self.tags.is_empty() {
            "-".to_owned()
        } else {
            self.tags.join(",")
        }
    }

    /// The `SOURCE` column, with markers for deprecated and hidden routes.
    fn source(&self) -> String {
        let mut out = self.source.clone().unwrap_or_else(|| "-".to_owned());
        if self.deprecated {
            out.push_str("  (deprecated)");
        }
        if self.hidden {
            out.push_str("  (hidden)");
        }
        out
    }
}

/// Run `moso routes`.
///
/// # Errors
/// Anything the dump protocol can fail with, plus a `--tag` that matches
/// nothing — a filter that silently prints an empty table is a filter that
/// wastes someone's afternoon.
pub fn run(ui: &Ui, args: &RoutesArgs) -> Outcome<()> {
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;
    let answer = project.dump(&args.app, Dump::Routes)?;
    let routes = parse(&answer)?;
    let shown = filter(&routes, args.tag.as_deref(), args.all);

    if let Some(tag) = &args.tag
        && shown.is_empty()
    {
        let mut known: Vec<&str> = routes
            .iter()
            .flat_map(|route| route.tags.iter().map(String::as_str))
            .collect();
        known.sort_unstable();
        known.dedup();
        return Err(
            CliError::user(format!("no route carries the tag `{tag}`")).with_help(
                if known.is_empty() {
                    "no route carries a tag; add one with `.tag(\"users\")` on the router"
                        .to_owned()
                } else {
                    format!("the tags in use are: {}", known.join(", "))
                },
            ),
        );
    }

    if ui.is_json() {
        ui.emit_json(&serde_json::json!({
            "ok": true,
            "routes": shown.iter().map(|route| serde_json::json!({
                "method": route.method,
                "path": route.path,
                "handler": route.handler,
                "security": route.security,
                "tags": route.tags,
                "source": route.source,
                "documented": route.documented,
                "hidden": route.hidden,
                "deprecated": route.deprecated,
            })).collect::<Vec<_>>(),
            "total": routes.len(),
        }));
        return Ok(());
    }

    if shown.is_empty() {
        ui.warn("this application registers no routes");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|route| {
            vec![
                route.method(),
                route.path.clone(),
                route.handler.clone(),
                route.auth(),
                route.tags(),
                route.source(),
            ]
        })
        .collect();

    ui.blank();
    ui.table(
        &["METHOD", "PATH", "HANDLER", "AUTH", "TAGS", "SOURCE"],
        &rows,
    );
    ui.blank();

    let undocumented = shown.iter().filter(|route| !route.documented).count();
    if undocumented > 0 {
        ui.status(
            Level::Warn,
            &format!("{undocumented} of {} routes are undocumented", shown.len()),
            "(registered without `#[endpoint]`)",
        );
        ui.fix("put `#[endpoint]` on the handler and register it with `routes!`");
    }
    let hidden = routes.len() - shown.len();
    if hidden > 0 && !args.all {
        ui.status(
            Level::Info,
            &format!("{hidden} hidden from this table"),
            "(pass --all to see them)",
        );
    }

    Ok(())
}

/// Parse `{"routes": [ .. ]}`.
fn parse(answer: &str) -> Outcome<Vec<Route>> {
    let value: Value = serde_json::from_str(answer).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-routes` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })?;

    let routes = value
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliError::user("the application's `--dump-routes` output has no `routes` array")
                .with_help("compare src/dump.rs with the one `moso new` writes")
        })?;

    Ok(routes.iter().map(Route::from_json).collect())
}

/// Apply `--tag` and `--all`.
fn filter<'a>(routes: &'a [Route], tag: Option<&str>, all: bool) -> Vec<&'a Route> {
    routes
        .iter()
        .filter(|route| all || !route.hidden)
        .filter(|route| tag.is_none_or(|tag| route.tags.iter().any(|owned| owned == tag)))
        .collect()
}

/// A string field, or the empty string.
fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A string-array field, or an empty vector.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A boolean field, defaulting to false.
fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = r#"{
      "routes": [
        {"method":"GET","path":"/users","handler":"users::list","security":["session"],
         "tags":["users"],"source":"src/routes/users.rs:14","documented":true,
         "hidden":false,"deprecated":false},
        {"method":"POST","path":"/users","handler":"<undocumented>","security":[],
         "tags":[],"source":null,"documented":false,"hidden":false,"deprecated":true},
        {"method":"GET","path":"/_internal","handler":"internal","security":[],
         "tags":["ops"],"source":null,"documented":true,"hidden":true,"deprecated":false}
      ]
    }"#;

    #[test]
    fn every_field_survives_the_round_trip() {
        let routes = parse(ANSWER).expect("parsed");
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].method, "GET");
        assert_eq!(routes[0].path, "/users");
        assert_eq!(routes[0].handler, "users::list");
        assert_eq!(routes[0].security, vec!["session".to_owned()]);
        assert_eq!(routes[0].source.as_deref(), Some("src/routes/users.rs:14"));
        assert!(routes[0].documented);
        assert!(routes[1].deprecated);
        assert!(routes[2].hidden);
    }

    #[test]
    fn hidden_routes_are_filtered_out_unless_all_is_given() {
        let routes = parse(ANSWER).expect("parsed");
        assert_eq!(filter(&routes, None, false).len(), 2);
        assert_eq!(filter(&routes, None, true).len(), 3);
    }

    #[test]
    fn a_tag_filter_selects_and_a_hidden_route_still_needs_all() {
        let routes = parse(ANSWER).expect("parsed");
        assert_eq!(filter(&routes, Some("users"), false).len(), 1);
        assert_eq!(filter(&routes, Some("ops"), false).len(), 0);
        assert_eq!(filter(&routes, Some("ops"), true).len(), 1);
        assert_eq!(filter(&routes, Some("nope"), true).len(), 0);
    }

    #[test]
    fn the_method_column_is_http_cased_whatever_the_dump_said() {
        let routes = parse(r#"{"routes":[{"method":"get"},{"method":"DELETE"}]}"#).expect("parsed");
        assert_eq!(routes[0].method(), "GET");
        assert_eq!(routes[1].method(), "DELETE");
    }

    #[test]
    fn the_columns_render_a_dash_rather_than_a_blank() {
        let routes = parse(ANSWER).expect("parsed");
        assert_eq!(routes[0].auth(), "session");
        assert_eq!(routes[1].auth(), "-");
        assert_eq!(routes[1].tags(), "-");
        assert!(routes[1].source().starts_with('-'));
        assert!(routes[1].source().contains("deprecated"));
        assert!(routes[2].source().contains("hidden"));
    }

    #[test]
    fn a_missing_routes_array_is_a_user_error_pointing_at_the_protocol() {
        let error = parse(r#"{"nope": []}"#).expect_err("rejected");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.help.is_some_and(|help| help.contains("src/dump.rs")));
    }

    #[test]
    fn output_that_is_not_json_is_a_user_error() {
        assert!(parse("Compiling shop v0.1.0").is_err());
    }

    #[test]
    fn a_row_with_missing_fields_still_parses() {
        let routes = parse(r#"{"routes":[{"method":"GET"}]}"#).expect("parsed");
        assert_eq!(routes[0].method, "GET");
        assert_eq!(routes[0].path, "");
        assert!(!routes[0].documented);
        assert_eq!(routes[0].auth(), "-");
    }
}
