//! `moso middleware` — what actually wraps a request, in order.
//!
//! ```text
//!  #  MIDDLEWARE          SUMMARY
//!  1  catch_panic         render_details=false
//!  2  request_id          header=x-request-id generator=ulid
//! ```
//!
//! # Why this is two questions and not one
//!
//! A Moso request passes through two stacks. The **global** one is
//! `MiddlewareStack`, twelve ordered slots the application configures by name.
//! The **per-route** one is whatever `.layer()` and `.guard()` attached to an
//! individual entry, and it sits inside the global stack, closest to the
//! handler.
//!
//! Only the second one answers the question people actually have, which is "is
//! this route covered". `.layer()` applies to the routes registered *before* the
//! call, so a router function's chain has to be read positionally — and reading
//! it positionally is exactly what nobody does. So this command asks for both
//! (`--dump-middleware` and `--dump-routes`) and, with `--route`, lays them out
//! as one list from outermost to handler.
//!
//! # Why the CLI formats it and the application does not
//!
//! `MiddlewareStack::render()` already produces a block of text, and sending
//! that would have been one line here. It is the wrong half of the protocol to
//! own presentation: `--json` needs the fields rather than a paragraph, the
//! per-route table has to interleave data the stack does not have, and a
//! formatting fix would otherwise mean regenerating `src/dump.rs` in every
//! project that already exists. The application sends structure; this decides
//! what it looks like.

use serde_json::Value;

use crate::cli::MiddlewareArgs;
use crate::exit::{CliError, Outcome};
use crate::project::{Dump, Project};
use crate::ui::{Level, Ui};

/// One slot of the global stack, as the application described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its position, outermost first.
    pub position: usize,
    /// The name printed in the table.
    pub name: String,
    /// Whether it will be applied.
    pub enabled: bool,
    /// A one-line summary of its configuration.
    pub summary: String,
    /// Whether it is a built-in slot rather than a custom layer.
    pub builtin: bool,
}

/// One route, read for the two fields this command needs.
///
/// Deliberately not the richer row `moso routes` prints: this table answers
/// "what wraps it", so the tags, the security schemes and the source location
/// would be three columns of noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// The HTTP method, upper-cased for display.
    pub method: String,
    /// The full path, prefixes applied.
    pub path: String,
    /// The handler's name.
    pub handler: String,
    /// The layers attached to this entry, innermost first.
    pub layers: Vec<String>,
    /// How many guards protect it.
    pub guards: u64,
}

impl Wrapped {
    /// The layers outermost first, which is the order a request meets them.
    ///
    /// The dump carries them innermost first, because that is the order they
    /// were pushed. Both orders are correct and only one of them reads like a
    /// stack, so the reversal happens once, here.
    fn outermost_first(&self) -> Vec<String> {
        let mut layers = self.layers.clone();
        layers.reverse();
        layers
    }

    /// The `LAYERS` column.
    fn layers_column(&self) -> String {
        if self.layers.is_empty() {
            "-".to_owned()
        } else {
            self.outermost_first().join(" → ")
        }
    }
}

/// Run `moso middleware`.
///
/// # Errors
/// Anything the dump protocol can fail with, plus a `--route` that matches no
/// registered route — printing an empty stack for a path that does not exist
/// would answer a question the reader did not ask.
pub fn run(ui: &Ui, args: &MiddlewareArgs) -> Outcome<()> {
    let project = Project::discover(args.app.manifest_path.as_deref())?;
    project.require_moso()?;

    let stack = parse_stack(&project.dump(&args.app, Dump::Middleware)?)?;
    let routes = parse_routes(&project.dump(&args.app, Dump::Routes)?)?;

    let selected: Vec<&Wrapped> = match &args.route {
        Some(path) => matching(&routes, path),
        None => routes.iter().collect(),
    };

    if let Some(path) = &args.route
        && selected.is_empty()
    {
        return Err(
            CliError::user(format!("no route matches `{path}`")).with_help(
                "run `moso routes` to see the paths this application registers; the filter \
                 matches a whole path or any part of one",
            ),
        );
    }

    if ui.is_json() {
        ui.emit_json(&json(&stack, &selected, args.all));
        return Ok(());
    }

    match &args.route {
        Some(_) => effective(ui, &stack, &selected),
        None => overview(ui, &stack, &routes, args.all),
    }
    Ok(())
}

/// The default view: the global stack, then who has extra layers.
fn overview(ui: &Ui, stack: &[Entry], routes: &[Wrapped], all: bool) {
    let shown: Vec<&Entry> = stack.iter().filter(|entry| all || entry.enabled).collect();

    ui.blank();
    ui.heading("GLOBAL");
    if shown.is_empty() {
        ui.warn("this application composes no middleware at all");
        ui.fix("`MiddlewareStack::empty()` was used; `MiddlewareStack::standard()` is the default");
    } else {
        let rows: Vec<Vec<String>> = shown
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                vec![
                    (index + 1).to_string(),
                    entry.name.clone(),
                    if entry.enabled { "yes" } else { "no" }.to_owned(),
                    detail(&entry.summary),
                ]
            })
            .collect();
        ui.table(&["#", "MIDDLEWARE", "ON", "SUMMARY"], &rows);
    }

    let layered: Vec<&Wrapped> = routes
        .iter()
        .filter(|route| !route.layers.is_empty() || route.guards > 0)
        .collect();

    ui.blank();
    ui.heading("PER ROUTE");
    if layered.is_empty() {
        ui.line(&ui.dim("  no route carries its own layers or guards"));
    } else {
        let rows: Vec<Vec<String>> = layered
            .iter()
            .map(|route| {
                vec![
                    route.method.clone(),
                    route.path.clone(),
                    route.layers_column(),
                    route.guards.to_string(),
                ]
            })
            .collect();
        ui.table(&["METHOD", "PATH", "LAYERS", "GUARDS"], &rows);
    }

    ui.blank();
    let bare = routes.len() - layered.len();
    if bare > 0 {
        ui.status(
            Level::Info,
            &format!(
                "{bare} of {} routes carry only the global stack",
                routes.len()
            ),
            "(pass --route <PATH> for one route's effective order)",
        );
    }
    let disabled = stack.iter().filter(|entry| !entry.enabled).count();
    if disabled > 0 && !all {
        ui.status(
            Level::Info,
            &format!("{disabled} slots present but disabled"),
            "(pass --all to see them)",
        );
    }
}

/// The `--route` view: one list per route, outermost to handler.
fn effective(ui: &Ui, stack: &[Entry], routes: &[&Wrapped]) {
    for route in routes {
        ui.blank();
        ui.heading(&format!("{} {}", route.method, route.path));

        let mut rows: Vec<Vec<String>> = Vec::new();
        // Disabled slots are never listed here whatever `--all` says: this view
        // claims to be what a request meets, and a slot that is off is not on
        // the path. The overview is where "present but disabled" belongs.
        for entry in stack.iter().filter(|entry| entry.enabled) {
            rows.push(vec![
                (rows.len() + 1).to_string(),
                "global".to_owned(),
                entry.name.clone(),
                detail(&entry.summary),
            ]);
        }
        for layer in route.outermost_first() {
            rows.push(vec![
                (rows.len() + 1).to_string(),
                "route".to_owned(),
                layer,
                "-".to_owned(),
            ]);
        }
        rows.push(vec![
            (rows.len() + 1).to_string(),
            "handler".to_owned(),
            route.handler.clone(),
            if route.guards == 0 {
                "-".to_owned()
            } else {
                format!("{} guard(s) run before it", route.guards)
            },
        ]);

        ui.table(&["#", "SCOPE", "MIDDLEWARE", "SUMMARY"], &rows);
    }
    ui.blank();
}

/// The `--json` document.
fn json(stack: &[Entry], routes: &[&Wrapped], all: bool) -> Value {
    serde_json::json!({
        "ok": true,
        "global": stack
            .iter()
            .filter(|entry| all || entry.enabled)
            .map(|entry| serde_json::json!({
                "position": entry.position,
                "name": entry.name,
                "enabled": entry.enabled,
                "summary": entry.summary,
                "builtin": entry.builtin,
            }))
            .collect::<Vec<_>>(),
        "routes": routes
            .iter()
            .map(|route| serde_json::json!({
                "method": route.method,
                "path": route.path,
                "handler": route.handler,
                // Outermost first, matching the order the table prints and the
                // order a request meets them.
                "layers": route.outermost_first(),
                "guards": route.guards,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The routes `--route <PATH>` selects.
///
/// An exact path first, and only if nothing matches exactly does it fall back to
/// a substring. `--route /users` should mean the `/users` routes rather than
/// every path that happens to contain the word, and `--route users` should still
/// find something.
fn matching<'a>(routes: &'a [Wrapped], path: &str) -> Vec<&'a Wrapped> {
    let exact: Vec<&Wrapped> = routes.iter().filter(|route| route.path == path).collect();
    if !exact.is_empty() {
        return exact;
    }
    routes
        .iter()
        .filter(|route| route.path.contains(path))
        .collect()
}

/// An empty summary reads as a dash, not as a hole in the table.
fn detail(summary: &str) -> String {
    if summary.trim().is_empty() {
        "-".to_owned()
    } else {
        summary.to_owned()
    }
}

/// Parse `{"middleware": [ .. ]}`.
fn parse_stack(answer: &str) -> Outcome<Vec<Entry>> {
    let value: Value = serde_json::from_str(answer).map_err(|error| {
        CliError::user(format!(
            "the application's `--dump-middleware` output is not JSON: {error}"
        ))
        .with_help("everything except the document must go to stderr")
    })?;

    let entries = value
        .get("middleware")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CliError::user("the application's `--dump-middleware` output has no `middleware` array")
                .with_help(
                    "this project predates `moso middleware`; copy `fn middleware` and the \
                     `Dump::Middleware` arm from the src/dump.rs a fresh `moso new` writes",
                )
        })?;

    Ok(entries
        .iter()
        .enumerate()
        .map(|(index, entry)| Entry {
            position: entry
                .get("position")
                .and_then(Value::as_u64)
                .map_or(index, |position| position as usize),
            name: text(entry, "name"),
            // Absent reads as enabled: every slot the stack reports is one it
            // intends to run, and a missing field must not silently hide a layer
            // that is on.
            enabled: entry
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            summary: text(entry, "summary"),
            builtin: entry
                .get("builtin")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect())
}

/// Parse the two fields of `--dump-routes` this command reads.
fn parse_routes(answer: &str) -> Outcome<Vec<Wrapped>> {
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

    Ok(routes
        .iter()
        .map(|route| Wrapped {
            method: text(route, "method").to_uppercase(),
            path: text(route, "path"),
            handler: text(route, "handler"),
            layers: route
                .get("layers")
                .and_then(Value::as_array)
                .map(|layers| {
                    layers
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            guards: route.get("guards").and_then(Value::as_u64).unwrap_or(0),
        })
        .collect())
}

/// A string field, or the empty string.
fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STACK: &str = r#"{
      "middleware": [
        {"position":0,"name":"catch_panic","enabled":true,"summary":"render_details=false",
         "builtin":true},
        {"position":1,"name":"compression","enabled":false,"summary":"br,gzip","builtin":true},
        {"position":2,"name":"tenant","enabled":true,"summary":"","builtin":false}
      ]
    }"#;

    const ROUTES: &str = r#"{
      "routes": [
        {"method":"get","path":"/users","handler":"list","layers":["Throttle","Audit"],
         "guards":1},
        {"method":"post","path":"/users","handler":"create","layers":[],"guards":0}
      ]
    }"#;

    #[test]
    fn every_stack_field_survives_the_round_trip() {
        let stack = parse_stack(STACK).expect("parsed");
        assert_eq!(stack.len(), 3);
        assert_eq!(stack[0].name, "catch_panic");
        assert_eq!(stack[0].summary, "render_details=false");
        assert!(stack[0].builtin);
        assert!(!stack[1].enabled);
        assert!(!stack[2].builtin);
    }

    #[test]
    fn a_slot_that_does_not_say_whether_it_is_on_is_treated_as_on() {
        // Failing open here is the safe direction: the risk is showing a layer
        // that will not run, and the alternative risks hiding one that will.
        let stack = parse_stack(r#"{"middleware":[{"name":"trace"}]}"#).expect("parsed");
        assert!(stack[0].enabled);
        assert_eq!(stack[0].position, 0);
    }

    #[test]
    fn layers_are_reversed_into_the_order_a_request_meets_them() {
        let routes = parse_routes(ROUTES).expect("parsed");
        assert_eq!(routes[0].layers, vec!["Throttle", "Audit"]);
        assert_eq!(routes[0].outermost_first(), vec!["Audit", "Throttle"]);
        assert_eq!(routes[0].layers_column(), "Audit → Throttle");
        assert_eq!(routes[1].layers_column(), "-");
    }

    #[test]
    fn the_method_column_is_http_cased_whatever_the_dump_said() {
        let routes = parse_routes(ROUTES).expect("parsed");
        assert_eq!(routes[0].method, "GET");
        assert_eq!(routes[1].method, "POST");
    }

    #[test]
    fn an_exact_path_wins_over_a_substring() {
        let routes = vec![
            Wrapped {
                method: "GET".to_owned(),
                path: "/users".to_owned(),
                handler: "list".to_owned(),
                layers: Vec::new(),
                guards: 0,
            },
            Wrapped {
                method: "GET".to_owned(),
                path: "/users/{id}".to_owned(),
                handler: "show".to_owned(),
                layers: Vec::new(),
                guards: 0,
            },
        ];
        assert_eq!(matching(&routes, "/users").len(), 1);
        assert_eq!(matching(&routes, "user").len(), 2);
        assert!(matching(&routes, "/posts").is_empty());
    }

    #[test]
    fn the_json_document_carries_the_stack_and_the_routes() {
        let stack = parse_stack(STACK).expect("parsed");
        let routes = parse_routes(ROUTES).expect("parsed");
        let selected: Vec<&Wrapped> = routes.iter().collect();

        let hidden = json(&stack, &selected, false);
        assert_eq!(hidden["global"].as_array().expect("array").len(), 2);

        let shown = json(&stack, &selected, true);
        assert_eq!(shown["global"].as_array().expect("array").len(), 3);
        assert_eq!(shown["routes"][0]["layers"][0], "Audit");
        assert_eq!(shown["routes"][0]["guards"], 1);
    }

    #[test]
    fn a_missing_middleware_array_points_at_the_half_of_the_protocol_to_copy() {
        let error = parse_stack(r#"{"nope": []}"#).expect_err("rejected");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert!(error.help.is_some_and(|help| help.contains("src/dump.rs")));
    }

    #[test]
    fn output_that_is_not_json_is_a_user_error() {
        assert!(parse_stack("Compiling shop v0.1.0").is_err());
        assert!(parse_routes("Compiling shop v0.1.0").is_err());
    }

    #[test]
    fn an_empty_summary_renders_as_a_dash() {
        assert_eq!(detail(""), "-");
        assert_eq!(detail("  "), "-");
        assert_eq!(detail("timeout 30s"), "timeout 30s");
    }
}
