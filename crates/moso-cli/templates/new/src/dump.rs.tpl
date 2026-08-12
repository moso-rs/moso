//! How the `moso` CLI talks to this application.
//!
//! `moso routes`, `moso openapi export`, `moso openapi check`, `moso config`,
//! `moso middleware`, `moso check`, `moso jobs` and `moso authz` do not link
//! your crate and do not parse your source. They run
//!
//! ```text
//! cargo run --quiet -- --dump-<kind>
//! ```
//!
//! and read exactly one document off standard output. That is why this file is
//! in your project and not inside the framework: the CLI asks, your `main`
//! answers, and you can see — and change — what it answers with.
//!
//! | flag                       | standard output                          |
//! | -------------------------- | ---------------------------------------- |
//! | `--dump-openapi`           | the OpenAPI document, as JSON            |
//! | `--dump-routes`            | `{"routes": [ .. ]}`                     |
//! | `--dump-config`            | `{"profile": .., "entries": [ .. ]}`     |
//! | `--dump-env-example`       | the text of `.env.example`               |
//! | `--dump-middleware`        | `{"middleware": [ .. ]}`                 |
//! | `--dump-jobs <request>`    | `{"available": .., "queues": [ .. ]}`    |
//! | `--dump-authz <request>`   | `{"available": .., "view": .., ..}`      |
//! | `--dump-auth <request>`    | `{"available": .., "params": .., ..}`    |
//!
//! Everything else the process writes — logs, warnings, panics — must go to
//! standard error, or the CLI cannot parse the answer. Moso's tracing layer
//! already writes to stderr, so this holds by default.
//!
//! # Why three of them take an argument
//!
//! The first five questions are pure functions of an application that has
//! already been built, so the flag alone is the whole question. The last three
//! are not: `moso jobs dlq --job send_welcome --limit 50`,
//! `moso authz explain --actor usr_1 --action publish` and
//! `moso auth calibrate --target-ms 250` carry parameters, and one of them
//! (`retry`, `discard`) *changes something*. Rather than growing a flag per
//! parameter, the CLI passes one JSON **request document** as the next argument
//! and this file reads it. Adding a filter is then a field, not a flag, and the
//! two halves cannot drift out of step over argument order.
//!
//! A request that is absent or unparseable arrives as `null`, and every renderer
//! echoes back the `request` it understood so a mismatch is visible rather than
//! silent.
//!
//! # Batteries this project does not use
//!
//! `--dump-jobs`, `--dump-authz` and `--dump-auth` are answered here whether or
//! not the battery behind each one is wired, and an unwired one answers
//! `{"available": false, ..}` with the reason and the fix.
//!
//! That is deliberate, and it is not the same thing as pretending. If `main` did
//! not recognise the flag it would fall through to `serve()`, and `moso jobs
//! status` would sit there until it timed out and then report a hung binary
//! rather than a battery you have not wired. One honest document turns a
//! sixty-second mystery into a sentence. The commented body above each renderer
//! is the code to paste in once the battery *is* wired.

use moso::deps::serde_json::{Value, json};
use moso::prelude::*;

/// The questions the CLI can ask.
///
/// Not `Copy`: the last two carry the request document described in the module
/// header.
#[derive(Debug, Clone, PartialEq)]
pub enum Dump {
    /// The OpenAPI document.
    OpenApi,
    /// The route table.
    Routes,
    /// The resolved configuration, with the origin of every value.
    Config,
    /// The regenerated `.env.example`.
    EnvExample,
    /// The composed middleware stack, outermost first.
    Middleware,
    /// The operator's view of the background queues.
    Jobs(Value),
    /// Permissions, roles, and why one decision went the way it did.
    Authz(Value),
    /// The argon2id parameters this machine can afford.
    Auth(Value),
}

impl Dump {
    /// Every flag this file answers, in the order the table above lists them.
    ///
    /// One array rather than a match on both sides: `requested` needs to *find*
    /// a flag among the arguments before it can decode one, and a list that can
    /// disagree with the decoder is how a flag ends up recognised by exactly one
    /// of the two.
    pub const FLAGS: &'static [&'static str] = &[
        "--dump-openapi",
        "--dump-routes",
        "--dump-config",
        "--dump-env-example",
        "--dump-middleware",
        "--dump-jobs",
        "--dump-authz",
        "--dump-auth",
    ];

    /// Decode one flag and the request that may follow it.
    pub fn parse(flag: &str, request: Option<&str>) -> Option<Self> {
        match flag {
            "--dump-openapi" => Some(Self::OpenApi),
            "--dump-routes" => Some(Self::Routes),
            "--dump-config" => Some(Self::Config),
            "--dump-env-example" => Some(Self::EnvExample),
            "--dump-middleware" => Some(Self::Middleware),
            "--dump-jobs" => Some(Self::Jobs(request_document(request))),
            "--dump-authz" => Some(Self::Authz(request_document(request))),
            "--dump-auth" => Some(Self::Auth(request_document(request))),
            _ => None,
        }
    }
}

/// The dump the command line asked for, if any.
pub fn requested() -> Option<Dump> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let position = arguments
        .iter()
        .position(|argument| Dump::FLAGS.contains(&argument.as_str()))?;

    // A following argument is the request only if it is not itself a flag, so
    // `--dump-jobs --verbose` reads as "no request" rather than as a request
    // that happens to be unparseable.
    let request = arguments
        .get(position + 1)
        .filter(|next| !next.starts_with("--"))
        .map(String::as_str);

    Dump::parse(&arguments[position], request)
}

/// Read the request document that follows a flag.
///
/// `null` when it is absent or is not JSON: the renderers all have a defensible
/// default view, and refusing to answer at all would turn a typo into a hang.
fn request_document(request: Option<&str>) -> Value {
    request
        .and_then(|text| moso::deps::serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

/// Answer `dump` on standard output.
///
/// `async` because of the last three: a queue's depth, a role source and an
/// argon2 measurement are all things that happen *now* rather than facts about
/// an application that has already been built. The first five ignore it.
///
/// # Errors
/// When the document cannot be serialised, or when the configuration sources
/// cannot be read.
pub async fn run(dump: Dump, app: &App) -> Result<()> {
    let rendered = match dump {
        Dump::OpenApi => app.openapi().to_json_pretty().map_err(Error::internal)?,
        Dump::Routes => to_json(&routes(app))?,
        Dump::Config => to_json(&config()?)?,
        Dump::EnvExample => crate::AppConfig::descriptor().render_env_example(crate::ENV_PREFIX),
        Dump::Middleware => to_json(&middleware(app))?,
        Dump::Jobs(request) => to_json(&jobs(&request))?,
        Dump::Authz(request) => to_json(&authz(&request))?,
        Dump::Auth(request) => to_json(&auth(&request).await)?,
    };
    // Exactly one document and exactly one trailing newline: `moso config
    // --env-example --out .env.example` has to be a no-op when nothing changed,
    // and a stray blank line would show up as a diff every time.
    println!("{}", rendered.trim_end());
    Ok(())
}

/// One row per registered route, in registration order.
fn routes(app: &App) -> Value {
    let rows: Vec<Value> = app
        .router_info()
        .iter()
        .map(|route| {
            json!({
                "method": route.method.as_str(),
                "path": route.path,
                "handler": route.handler,
                "operation_id": route.operation_id,
                "summary": route.summary,
                "tags": route.tags,
                "security": route.security,
                "source": route.source.map(|at| at.to_string()),
                "documented": route.documented,
                "deprecated": route.deprecated,
                "hidden": route.hidden,
                "guards": route.guards,
                "layers": route.layers,
            })
        })
        .collect();

    json!({ "routes": rows })
}

/// The composed middleware stack, outermost first.
///
/// The structured entries rather than `MiddlewareStack::render()`, which is the
/// same data already formatted. The CLI owns presentation — it has to lay the
/// global stack and the per-route layers out together, and `moso middleware
/// --json` needs the fields, not a paragraph.
///
/// Disabled entries are included and flagged. "`compression` is off" and
/// "`compression` is not in this stack" are different facts, and only one of
/// them is a configuration you can change.
fn middleware(app: &App) -> Value {
    let entries: Vec<Value> = app
        .middleware_stack()
        .describe()
        .iter()
        .enumerate()
        .map(|(position, entry)| {
            json!({
                "position": position,
                "name": entry.name,
                "enabled": entry.enabled,
                "summary": entry.summary,
                // A built-in slot can be configured by name; a custom layer was
                // inserted by a line in your composition root.
                "builtin": entry.slot.is_some(),
            })
        })
        .collect();

    json!({ "middleware": entries })
}

/// The operator's view of the background queues.
///
/// This project does not depend on `moso-jobs`, so the honest answer is that
/// there is nothing to report and why. Once you add the battery, delete the body
/// below and paste this:
///
/// ```text
/// fn jobs(request: &Value) -> Value {
///     let Some(jobs) = moso::jobs::Jobs::installed() else {
///         return unavailable("jobs", "no `Jobs` handle is installed in this process",
///             "call `Jobs::new(queue, registry).install()` in `build()`");
///     };
///
///     let registered: Vec<Value> = jobs.registry().all().map(|job| json!({
///         "name": job.name(),
///         "type": job.type_name(),
///         "queue": job.queue(),
///         "timeout_seconds": job.timeout().as_secs(),
///         "priority": format!("{:?}", job.priority()),
///         "serial": job.serial(),
///     })).collect();
///
///     // `stats`, `schedule_runs`, `DeadLetterQueue::list`, `retry` and
///     // `discard` are all `async`. Make this function `async` too — `run`
///     // already is — and await them:
///     let queues = jobs.stats().await;
///     …
/// }
/// ```
///
/// The `action` field of the request is what makes this more than a dump:
/// `{"action":"retry","filter":{"job":".."},"limit":50}` calls
/// `DeadLetterQueue::retry` and answers with how many rows moved. Keep the
/// `limit` mandatory — a bulk operation over an unbounded filter is how a fix
/// becomes an outage.
fn jobs(request: &Value) -> Value {
    unavailable(
        request,
        "this project does not use moso-jobs, so it registers no jobs",
        "add `moso = { version = \"..\", features = [\"jobs\"] }` to Cargo.toml, register your \
         jobs with `JobRegistry::new().register::<MyJob>()`, then replace `fn jobs` in \
         src/dump.rs with the body in the comment above it",
    )
}

/// Permissions, roles, and why one decision went the way it did.
///
/// This project does not depend on `moso-authz`. Once you add it, the four views
/// the CLI asks for are:
///
/// ```text
/// fn authz(request: &Value) -> Value {
///     match request.get("view").and_then(Value::as_str).unwrap_or("check") {
///         // `moso check --authz`
///         "check" => json!({
///             "view": "check", "available": true,
///             "undeclared": moso_authz::undeclared_operations(app.openapi())
///                 .into_iter()
///                 .map(|(method, path, source)| json!({
///                     "method": method, "path": path, "source": source,
///                 }))
///                 .collect::<Vec<_>>(),
///             "problems": moso_authz::document_problems(app.openapi())
///                 .into_iter()
///                 .map(|(at, error)| json!({ "at": at, "message": error.to_string() }))
///                 .collect::<Vec<_>>(),
///         }),
///         // `moso authz permissions` — the registry and its fingerprint
///         "permissions" => { let registry = moso_authz::PermissionRegistry::of::<Perm>(); … }
///         // `moso authz roles`
///         "roles" => …,
///         // `moso authz explain` — build the Explanation and hand back
///         // `explanation.render()` *and* the structured form
///         "explain" => …,
///         other => json!({ "view": other, "available": false, "reason": "unknown view" }),
///     }
/// }
/// ```
///
/// # The one rule to keep when you replace this
///
/// The production refusal below is not scaffolding. An explain trace hands your
/// whole authorization model — the roles, the permissions each grants, the
/// policy that ran and its reason — to whoever asked for it, which is why the
/// `X-Moso-Authz-Explain` header is honoured in a development profile and
/// nowhere else. The offline entry point has to hold the same line, so it
/// refuses in production unless the caller passes `--allow-production` and it
/// says why. Keep that check ahead of whatever you build the explanation from.
fn authz(request: &Value) -> Value {
    let view = request
        .get("view")
        .and_then(Value::as_str)
        .unwrap_or("check");
    let profile = moso::config::Profile::detect();

    if view == "explain"
        && profile == moso::config::Profile::Production
        && request.get("allow_production").and_then(Value::as_bool) != Some(true)
    {
        return json!({
            "view": view,
            "available": false,
            "refused": true,
            "profile": profile.to_string(),
            "reason": "an explain trace describes the whole authorization model, so it is \
                       refused in the production profile",
            "help": "run it against a development profile, or pass --allow-production if you \
                     are certain this terminal is the right place for it",
        });
    }

    let mut document = unavailable(
        request,
        "this project does not use moso-authz, so it declares no permissions",
        "add `moso-authz` to Cargo.toml, declare your permissions with `moso::permissions!`, \
         then replace `fn authz` in src/dump.rs with the body in the comment above it",
    );
    document["view"] = json!(view);
    document["profile"] = json!(profile.to_string());
    document
}

/// The argon2id parameters this machine can afford, for `moso auth calibrate`.
///
/// It is answered by running *your* binary rather than by a table in the CLI
/// because the answer is a property of the hardware the hash will run on:
/// parameters that take 250 ms on a laptop take three times that in a container
/// with half a CPU, and a constant is wrong on both.
///
/// A project created with `moso new --auth` answers this from `src/auth.rs`. In
/// one that was not, the whole body is:
///
/// ```text
/// async fn auth(request: &Value) -> Value {
///     let target = std::time::Duration::from_millis(
///         request.get("target_ms").and_then(Value::as_u64).unwrap_or(250),
///     );
///     let params = match moso::auth::calibrate(target).await {
///         Ok(params) => params,
///         Err(error) => return json!({ "available": false, "reason": error.to_string() }),
///     };
///     json!({
///         "available": true, "request": request, "action": "calibrate",
///         "params": { "memory_kib": params.memory_kib, "iterations": params.iterations,
///                     "parallelism": params.parallelism },
///         // The floor travels with the answer, so the CLI does not keep a
///         // second copy of OWASP's minimum that could drift from this one.
///         "floor": { "memory_kib": moso::auth::HashParams::OWASP_MINIMUM.memory_kib,
///                    "iterations": moso::auth::HashParams::OWASP_MINIMUM.iterations,
///                    "parallelism": moso::auth::HashParams::OWASP_MINIMUM.parallelism },
///         // The keys *this* application reads them from, so what the CLI
///         // prints can be pasted rather than translated.
///         "config": [format!("{}__HASH_MEMORY_KIB={}", crate::ENV_PREFIX, params.memory_kib)],
///     })
/// }
/// ```
async fn auth(request: &Value) -> Value {
    @@AUTH_DUMP@@
}

/// The shape every "you have not wired this" answer takes.
///
/// One helper so the two batteries cannot describe their absence differently,
/// and so the CLI has exactly one field to branch on. `available` is the branch;
/// `reason` and `help` are what the CLI prints instead of an empty table.
fn unavailable(request: &Value, reason: &str, help: &str) -> Value {
    json!({
        "available": false,
        "request": request,
        "reason": reason,
        "help": help,
    })
}

/// Every configuration key, with the value that won and where it came from.
///
/// Resolution only — nothing is coerced, so a key holding an unusable value is
/// shown rather than hidden behind the error it would cause at boot.
fn config() -> Result<Value> {
    let loader = crate::loader()?;
    let resolved = crate::AppConfig::descriptor().resolve(&loader);

    let entries: Vec<Value> = resolved
        .entries
        .iter()
        .map(|entry| {
            json!({
                "key": entry.key.dotted(),
                "env": entry.key.env_name(crate::ENV_PREFIX),
                "value": entry.value,
                "origin": entry.origin.as_ref().map(ToString::to_string),
                "secret": entry.secret,
            })
        })
        .collect();

    Ok(json!({
        "profile": resolved.profile.to_string(),
        "entries": entries,
    }))
}

/// Serialise, turning a serialisation failure into a Moso error.
fn to_json(value: &Value) -> Result<String> {
    moso::deps::serde_json::to_string_pretty(value).map_err(Error::internal)
}
