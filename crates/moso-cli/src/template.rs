//! The project template, embedded in the binary.
//!
//! `include_str!` rather than an asset crate: the template is nine text files
//! that must ship inside a single static binary, and `include_str!` does that
//! for free with no build script, no dependency and no runtime lookup that can
//! fail. If the template ever grows a binary asset or a per-database variant,
//! that is the moment to reach for something bigger — not before.
//!
//! Substitution is deliberately not a template *language*. The placeholders are
//! spelled `@@LIKE_THIS@@` and the only operation is replacement. A generated
//! project has to be readable Rust that a person learns the framework from; if
//! the template needed conditionals, the template would be wrong.
//!
//! # The two variants
//!
//! `moso new --with-db` adds a migration story: a `migrations/` directory, a
//! `src/db.rs` implementing the `--db-*` protocol `moso db` speaks, a
//! `database_url` on the configuration, and the `moso-migrate` dependency.
//!
//! `moso new --auth` adds the authentication story: a `src/auth.rs` holding the
//! user type, the account store and seven `#[endpoint]` handlers copied out of
//! `moso-auth`, a `tests/auth.rs` that drives them over HTTP, a signing key and
//! the argon2id parameters on the configuration, and a `.env` carrying a key
//! generated from this machine's CSPRNG. It is the second of the battery's two
//! tiers (`03-batteries/30-auth.md`): the mounted `moso::auth::routes()` is
//! fixed to the framework's own user type, and this one is not.
//!
//! Both are expressed as *placeholders that expand to nothing* in the default
//! project rather than as a second copy of `Cargo.toml`, `lib.rs` and `main.rs`.
//! Two copies of a file that must stay in step is how a template rots: a fix
//! applied to one and not the other is invisible until somebody generates the
//! variant nobody tests. The cost is a dozen placeholders that are usually
//! empty, which is the cheaper of the two.

use std::path::{Path, PathBuf};

use crate::exit::{CliError, Outcome};

/// One file of the generated project.
#[derive(Debug, Clone, Copy)]
pub struct TemplateFile {
    /// Where it lands, relative to the project root.
    pub path: &'static str,
    /// Its contents, before substitution.
    pub contents: &'static str,
}

/// Every file `moso new` writes, in the order it writes them.
pub const FILES: &[TemplateFile] = &[
    TemplateFile {
        path: "Cargo.toml",
        contents: include_str!("../templates/new/Cargo.toml.tpl"),
    },
    TemplateFile {
        path: ".gitignore",
        contents: include_str!("../templates/new/gitignore.tpl"),
    },
    TemplateFile {
        path: ".env.example",
        contents: include_str!("../templates/new/env.example.tpl"),
    },
    TemplateFile {
        path: ".cargo/config.toml",
        contents: include_str!("../templates/new/cargo/config.toml.tpl"),
    },
    // `04-devex/40-cli.md` and M1's definition-of-done step 8 both require that
    // a generated project deploys as a single container image without the user
    // writing the Dockerfile themselves.
    TemplateFile {
        path: "Dockerfile",
        contents: include_str!("../templates/new/Dockerfile.tpl"),
    },
    TemplateFile {
        path: ".dockerignore",
        contents: include_str!("../templates/new/dockerignore.tpl"),
    },
    TemplateFile {
        path: "README.md",
        contents: include_str!("../templates/new/README.md.tpl"),
    },
    TemplateFile {
        path: "src/lib.rs",
        contents: include_str!("../templates/new/src/lib.rs.tpl"),
    },
    TemplateFile {
        path: "src/main.rs",
        contents: include_str!("../templates/new/src/main.rs.tpl"),
    },
    TemplateFile {
        path: "src/routes.rs",
        contents: include_str!("../templates/new/src/routes.rs.tpl"),
    },
    TemplateFile {
        path: "src/dump.rs",
        contents: include_str!("../templates/new/src/dump.rs.tpl"),
    },
    TemplateFile {
        path: "tests/api.rs",
        contents: include_str!("../templates/new/tests/api.rs.tpl"),
    },
];

/// The extra files `moso new --with-db` writes, on top of [`FILES`].
pub const DB_FILES: &[TemplateFile] = &[
    TemplateFile {
        path: "src/db.rs",
        contents: include_str!("../templates/new/src/db.rs.tpl"),
    },
    // A first migration that is real rather than empty: `moso db migrate` on a
    // fresh project has to *do* something, or its first run proves nothing.
    TemplateFile {
        path: "migrations/20260101T000000_init.sql",
        contents: include_str!("../templates/new/migrations/init.sql.tpl"),
    },
];

/// The extra files `moso new --auth` writes, on top of [`FILES`].
///
/// `.env` is among them, and it is the only file `moso new` ever writes that is
/// not committed: the session signing key is required configuration with no
/// default, so without it the generated project would not boot until the reader
/// had gone and found `moso config --generate-secret`. `.gitignore` already
/// excludes it, so the `git add --all` that follows cannot pick it up.
pub const AUTH_FILES: &[TemplateFile] = &[
    TemplateFile {
        path: "src/auth.rs",
        contents: include_str!("../templates/new/src/auth.rs.tpl"),
    },
    TemplateFile {
        path: "tests/auth.rs",
        contents: include_str!("../templates/new/tests/auth.rs.tpl"),
    },
    TemplateFile {
        path: ".env",
        contents: include_str!("../templates/new/env.tpl"),
    },
];

/// What the placeholders expand to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vars {
    /// The package name, as written in `Cargo.toml`.
    pub crate_name: String,
    /// The library name: `crate_name` with hyphens turned into underscores.
    pub lib_name: String,
    /// The environment-variable prefix: `lib_name` upper-cased.
    pub env_prefix: String,
    /// The `moso = ..` line of the generated `[dependencies]`.
    pub moso_dep: String,
    /// A `[workspace]` stanza, or the empty string.
    ///
    /// Present only when the new project would otherwise be swallowed by an
    /// enclosing workspace, which is the one case where the empty table is not
    /// noise but the difference between building and not.
    pub workspace: String,
    /// Whether `--with-db` was given.
    ///
    /// Drives the four `@@DB_*@@` placeholders and whether [`DB_FILES`] is
    /// written; see the module header for why it is a placeholder and not a
    /// second set of templates.
    pub with_db: bool,
    /// Whether `--auth` was given.
    ///
    /// Drives the `@@AUTH_*@@` placeholders and whether [`AUTH_FILES`] is
    /// written.
    pub with_auth: bool,
    /// The `--moso-path` a checkout was given as, escaped for TOML.
    moso_path: Option<String>,
    /// The `moso-migrate = ..` line, when a checkout is being used.
    ///
    /// Empty for a published build, where the version line is enough.
    moso_migrate_dep: String,
    /// The `moso-kv = ..` line, when a checkout is being used.
    ///
    /// Empty for a published build. Only `--auth` needs it: the lifecycle
    /// tokens live in a `moso_kv::Kv`, and nothing re-exports that type.
    moso_kv_dep: String,
    /// The base64 of the session signing key written into `.env`.
    ///
    /// Passed in rather than generated here, so that [`Vars`] stays a pure
    /// function of its inputs and a test can render the same bytes twice. The
    /// entropy comes from the operating system, in
    /// [`commands::secret`](crate::commands::secret).
    session_secret: String,
}

impl Vars {
    /// Derive every variable from a validated project name.
    ///
    /// # Errors
    /// [`Fault::User`](crate::exit::Fault::User) when the name is not usable as
    /// a Cargo package name.
    pub fn for_name(name: &str) -> Outcome<Self> {
        let crate_name = validate_name(name)?;
        let lib_name = crate_name.replace('-', "_");
        let env_prefix = lib_name.to_uppercase();
        Ok(Self {
            crate_name,
            lib_name,
            env_prefix,
            moso_dep: published_dependency(false),
            workspace: String::new(),
            with_db: false,
            with_auth: false,
            moso_path: None,
            moso_migrate_dep: String::new(),
            moso_kv_dep: String::new(),
            session_secret: String::new(),
        })
    }

    /// Depend on a Moso checkout on disk rather than on the published crate.
    ///
    /// What the CLI's own test suite uses, and what someone hacking on Moso
    /// itself wants. The path is written as given: a relative path stays
    /// relative, because that is what survives being committed.
    #[must_use]
    pub fn with_moso_path(mut self, path: &Path) -> Self {
        self.moso_path = Some(path.display().to_string().replace('\\', "\\\\"));
        // `moso-migrate` and `moso-kv` are siblings of `crates/moso` in the
        // checkout, and each is a separate crate rather than a feature of the
        // facade, so pointing one at a path and leaving the others on a version
        // would try to compile two different Mosos into one binary.
        self.moso_migrate_dep = sibling_dependency(path, "moso-migrate");
        self.moso_kv_dep = sibling_dependency(path, "moso-kv");
        self.moso_dep = self.dependency_line();
        self
    }

    /// Generate the migration story: `migrations/`, `src/db.rs`, `moso db`.
    #[must_use]
    pub fn with_database(mut self) -> Self {
        self.with_db = true;
        self
    }

    /// Generate the authentication story: `src/auth.rs`, `tests/auth.rs`, and
    /// the `.env` carrying `secret` as the session signing key.
    ///
    /// `secret` is the base64 of at least 32 bytes from the operating system's
    /// random number generator. It is an argument and not something this module
    /// produces, because a template renderer that reaches for entropy is a
    /// template renderer whose output cannot be asserted on.
    #[must_use]
    pub fn with_auth(mut self, secret: &str) -> Self {
        self.with_auth = true;
        self.session_secret = secret.to_owned();
        // The facade's `auth` feature is off by default, and turning it on is
        // what makes `moso::auth` exist at all.
        self.moso_dep = self.dependency_line();
        self
    }

    /// The `moso = ..` line for the flags given so far.
    ///
    /// Recomputed rather than edited in place, so that `--moso-path` and
    /// `--auth` compose whichever order they arrive in.
    fn dependency_line(&self) -> String {
        let Some(path) = &self.moso_path else {
            return published_dependency(self.with_auth);
        };
        if self.with_auth {
            format!("moso = {{ path = \"{path}\", features = [\"auth\", \"config-file\"] }}")
        } else {
            format!("moso = {{ path = \"{path}\", features = [\"config-file\"] }}")
        }
    }

    /// Emit the `[workspace]` stanza that detaches the project from an
    /// enclosing workspace.
    #[must_use]
    pub fn detached_from_workspace(mut self) -> Self {
        self.workspace = "\n# This directory sits inside another Cargo workspace. An empty\n\
                          # `[workspace]` table makes it a workspace root of its own, so it is\n\
                          # built and tested on its own terms. Delete it to join the outer one.\n\
                          [workspace]\n"
            .to_owned();
        self
    }

    /// Substitute one file.
    ///
    /// The `@@DB_*@@` expansions run **first**, because their replacements may
    /// themselves contain a scalar placeholder — `@@DB_ENV@@` expands to a line
    /// naming `@@ENV_PREFIX@@` — and a substitution that ran earlier cannot see
    /// text that a later one introduced.
    pub fn render(&self, source: &str) -> String {
        source
            .replace("@@DB_DEPS@@", self.db_deps().trim_end())
            .replace("@@DB_MOD@@", self.db_mod())
            .replace("@@DB_CONFIG@@", self.db_config())
            .replace("@@DB_DISPATCH@@", self.db_dispatch())
            .replace("@@DB_ENV@@", self.db_env())
            .replace("@@AUTH_DEPS@@", &self.auth_deps())
            .replace("@@AUTH_MOD@@", self.auth_mod())
            .replace("@@AUTH_CONFIG@@", self.auth_config())
            .replace("@@AUTH_SETUP@@", self.auth_setup())
            .replace("@@AUTH_WIRING@@", self.auth_wiring())
            .replace("@@AUTH_ENV@@", self.auth_env())
            .replace("@@AUTH_DUMP@@", self.auth_dump())
            .replace("@@SESSION_SECRET@@", &self.session_secret)
            .replace("@@CRATE_NAME@@", &self.crate_name)
            .replace("@@LIB_NAME@@", &self.lib_name)
            .replace("@@ENV_PREFIX@@", &self.env_prefix)
            .replace("@@MOSO_DEP@@", &self.moso_dep)
            .replace("@@WORKSPACE@@", &self.workspace)
    }

    /// The `[dependencies]` lines the migration story needs.
    fn db_deps(&self) -> String {
        if !self.with_db {
            return String::new();
        }
        let migrate = if self.moso_migrate_dep.is_empty() {
            format!("moso-migrate = \"{}\"", minor_version())
        } else {
            self.moso_migrate_dep.clone()
        };
        format!(
            "\n# The migration runner behind `moso db`. A separate crate and not a feature\n\
             # of the facade, because it pulls a database driver and the facade's default\n\
             # resolution is budgeted (00-foundations/03-crate-layout.md, rule 6).\n{migrate}\n"
        )
    }

    /// The `pub mod db;` line, or nothing.
    fn db_mod(&self) -> &'static str {
        if self.with_db { "\npub mod db;" } else { "" }
    }

    /// The `database_url` field on `AppConfig`, or nothing.
    fn db_config(&self) -> &'static str {
        if !self.with_db {
            return "";
        }
        "\n\n    /// Where the database is.\n    \
         //\n    \
         // `SecretString` and not `String`: a connection string carries a\n    \
         // password, and this type redacts itself in `Debug`, in `moso config`\n    \
         // and in any log line that formats the configuration. Reaching the\n    \
         // value is an explicit `.expose()`, which is greppable.\n    \
         #[config(secret)]\n    \
         pub database_url: SecretString,"
    }

    /// The `--db-*` branch of `main`, or nothing.
    ///
    /// Written with the library path spelled out rather than adding `db` to the
    /// `use` at the top: `main.rs` is the binary crate, so `crate::db` would not
    /// resolve, and a glob import to avoid naming it is the thing
    /// `01-goals.md` calls an anti-goal.
    fn db_dispatch(&self) -> &'static str {
        if !self.with_db {
            return "";
        }
        "\n    // `moso db status`, `migrate`, `rollback` and `redo` run this binary with\n    \
         // a `--db-*` flag and read one JSON document off stdout. See `src/db.rs`.\n    \
         if let Some(command) = @@LIB_NAME@@::db::requested() {\n        \
         return @@LIB_NAME@@::db::run(command).await;\n    \
         }\n"
    }

    /// The `DATABASE_URL` line of `.env.example`, or nothing.
    ///
    /// Hand-written to match what `#[derive(Config)]` renders byte for byte, and
    /// kept honest by the acceptance test that regenerates the file and diffs
    /// it. The `[required]` marker is part of that: the field has no
    /// `#[config(default = ..)]`, and the renderer says so.
    fn db_env(&self) -> &'static str {
        if !self.with_db {
            return "";
        }
        "\n\n# Where the database is.  [required]\n@@ENV_PREFIX@@__DATABASE_URL="
    }

    // ── the authentication variant ──────────────────────────────────────────

    /// The `[dependencies]` lines the authentication story needs.
    ///
    /// One crate, and it is not the facade: the lifecycle tokens live in a
    /// `moso_kv::Kv` and nothing re-exports that type, so a project that mints a
    /// password-reset token has to name the crate the store comes from. The
    /// `auth` feature itself rides on the `moso` line, which
    /// [`dependency_line`](Vars::dependency_line) writes.
    fn auth_deps(&self) -> String {
        if !self.with_auth {
            return String::new();
        }
        let kv = if self.moso_kv_dep.is_empty() {
            format!("moso-kv = \"{}\"", minor_version())
        } else {
            self.moso_kv_dep.clone()
        };
        format!(
            "\n# The store behind the single-use tokens a password reset mints. A separate\n\
             # crate because a stateless service should be able to have a cache without an\n\
             # ORM (02-data/25-kv-cache.md); the in-memory backend is its default feature.\n\
             {kv}"
        )
    }

    /// The `pub mod auth;` line, or nothing.
    fn auth_mod(&self) -> &'static str {
        if self.with_auth {
            "\npub mod auth;"
        } else {
            ""
        }
    }

    /// The session key and the argon2id parameters on `AppConfig`, or nothing.
    ///
    /// `SecretBytes` is spelled in full rather than imported: it is not in the
    /// prelude, and a variant that edits the `use` at the top of the file is a
    /// variant that conflicts with every other edit the reader makes there.
    fn auth_config(&self) -> &'static str {
        if !self.with_auth {
            return "";
        }
        "\n\n    /// The key the session cookie is signed with, at least 32 bytes.\n    \
         //\n    \
         // Required, with no default, and deliberately: a signing key with a\n    \
         // value in the source is a signing key everybody has. `moso new --auth`\n    \
         // wrote one into `.env` from this machine's CSPRNG; a deployment sets\n    \
         // its own. Rotating it logs everybody out, which is what you want the\n    \
         // day it leaks.\n    \
         #[config(secret)]\n    \
         pub session_secret: moso::config::SecretBytes,\n\n    \
         /// argon2id memory cost in kibibytes, from `moso auth calibrate`.\n    \
         //\n    \
         // The three below default to OWASP's minimum, which is the floor and\n    \
         // not a target: `moso auth calibrate` measures what this machine can\n    \
         // afford in 250 ms and prints the three lines to paste. Anything below\n    \
         // the floor is raised back to it rather than obeyed.\n    \
         #[config(default = \"19456\")]\n    \
         pub hash_memory_kib: u32,\n\n    \
         /// argon2id passes, from `moso auth calibrate`.\n    \
         #[config(default = \"2\")]\n    \
         pub hash_iterations: u32,\n\n    \
         /// argon2id lanes, from `moso auth calibrate`.\n    \
         #[config(default = \"1\")]\n    \
         pub hash_parallelism: u32,"
    }

    /// The two lines of `build()` that construct the auth state, or nothing.
    fn auth_setup(&self) -> &'static str {
        if !self.with_auth {
            return "";
        }
        "\n    // Authentication. The user type, the account store, the seven\n    \
         // handlers and the hashing parameters are all `src/auth.rs`, which is\n    \
         // yours to edit — that is the whole point of `moso new --auth`.\n    \
         let auth = auth::Auth::in_memory(&config)?;\n    \
         let session_layer = auth.session_layer(&config)?;\n    \
         // The credential the three authenticated routes declare. Its name\n    \
         // depends on the profile, so it is read from the live configuration\n    \
         // rather than written out here.\n    \
         let session_scheme = auth.session_scheme();\n"
    }

    /// The builder calls that install the session layer and mount the routes.
    fn auth_wiring(&self) -> &'static str {
        if !self.with_auth {
            return "";
        }
        "\n        .with_middleware(|stack| {\n            \
         // Nothing installs a session layer for you: it needs a store and a\n            \
         // key set that only this function has. `Slot::Session` is a reserved\n            \
         // position with no built-in, which is what `replace_custom` fills.\n            \
         stack.replace_custom(moso::middleware::Slot::Session, session_layer);\n        \
         })\n        .openapi(move |document| {\n            \
         document.security_scheme(moso::auth::extract::SESSION_SCHEME, session_scheme);\n        \
         })\n        .provide(auth)\n        .mount(auth::router())"
    }

    /// The `.env.example` lines for the auth configuration, or nothing.
    ///
    /// Hand-written to match what `#[derive(Config)]` renders byte for byte, for
    /// the same reason [`db_env`](Vars::db_env) is, and kept honest by the same
    /// acceptance test. A `#[config(secret)]` field is rendered with no value
    /// even when it has a default, so the first line ends in `=`.
    fn auth_env(&self) -> &'static str {
        if !self.with_auth {
            return "";
        }
        "\n\n# The key the session cookie is signed with, at least 32 bytes.  [required]\n\
         @@ENV_PREFIX@@__SESSION_SECRET=\n\n\
         # argon2id memory cost in kibibytes, from `moso auth calibrate`.\n\
         @@ENV_PREFIX@@__HASH_MEMORY_KIB=19456\n\n\
         # argon2id passes, from `moso auth calibrate`.\n\
         @@ENV_PREFIX@@__HASH_ITERATIONS=2\n\n\
         # argon2id lanes, from `moso auth calibrate`.\n\
         @@ENV_PREFIX@@__HASH_PARALLELISM=1"
    }

    /// The body of `dump::auth`: the real measurement, or the honest refusal.
    fn auth_dump(&self) -> &'static str {
        if self.with_auth {
            return "crate::auth::calibrate(request).await";
        }
        "unavailable(\n        request,\n        \
         \"this project does not use moso-auth, so there is nothing to calibrate\",\n        \
         \"create the project with `moso new --auth`, or add \\\n         \
         `moso = { version = \\\"..\\\", features = [\\\"auth\\\"] }` to Cargo.toml and replace \\\n         \
         `fn auth` in src/dump.rs with the body in the comment above it\",\n    )"
    }

    /// Every file of the project, rendered, in write order.
    pub fn render_all(&self) -> Vec<(PathBuf, String)> {
        FILES
            .iter()
            .chain(if self.with_db { DB_FILES } else { &[] })
            .chain(if self.with_auth { AUTH_FILES } else { &[] })
            .map(|file| (PathBuf::from(file.path), self.render(file.contents)))
            .collect()
    }
}

/// The path line for a crate sitting beside `crates/moso` in a checkout.
fn sibling_dependency(moso: &Path, name: &str) -> String {
    let sibling = moso
        .parent()
        .map_or_else(|| PathBuf::from(name), |at| at.join(name));
    let rendered = sibling.display().to_string().replace('\\', "\\\\");
    format!("{name} = {{ path = \"{rendered}\" }}")
}

/// The `major.minor` of this CLI, which is what a generated manifest pins to.
fn minor_version() -> String {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
}

/// The dependency line for a project that uses the published crate.
///
/// Pinned to the CLI's own minor version: a CLI and the framework it scaffolds
/// for are released together, and a template that generates code against a
/// version the CLI has never seen is a template that generates code that does
/// not compile.
fn published_dependency(with_auth: bool) -> String {
    let minor = minor_version();
    if with_auth {
        // `auth` is off by default and implies `orm`, because a user lives in a
        // table. Naming it is what makes `moso::auth` exist. `config-file` gives
        // the generated app the `config/*.toml` layers its `lib.rs` documents
        // (RFC-0001 makes it off-by-default on the facade to keep `cargo add
        // moso` lean).
        format!("moso = {{ version = \"{minor}\", features = [\"auth\", \"config-file\"] }}")
    } else {
        format!("moso = {{ version = \"{minor}\", features = [\"config-file\"] }}")
    }
}

/// Rust keywords a crate name cannot be, because the `use` in `main.rs` would
/// not parse.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro", "override",
    "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Names Cargo itself refuses, plus the two standard-library crates whose name
/// a dependency cannot shadow.
const RESERVED: &[&str] = &[
    "test",
    "core",
    "std",
    "alloc",
    "proc-macro",
    "build",
    "deps",
];

/// Check that `name` is usable as both a Cargo package name and a Rust
/// identifier, and normalise it.
///
/// # Errors
/// [`Fault::User`](crate::exit::Fault::User), with the specific reason and a
/// suggested replacement.
pub fn validate_name(name: &str) -> Outcome<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CliError::user("a project name cannot be empty").with_help("moso new my-api"));
    }
    if trimmed.len() > 64 {
        return Err(CliError::user(format!(
            "`{trimmed}` is {} characters; a crate name must be at most 64",
            trimmed.len()
        )));
    }

    let lowered = trimmed.to_ascii_lowercase();

    let bad: Vec<char> = lowered
        .chars()
        .filter(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
        .collect();
    if !bad.is_empty() {
        let suggestion = suggest(&lowered);
        return Err(CliError::user(format!(
            "`{trimmed}` is not a crate name: {} is not allowed",
            bad.iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .with_help(format!("moso new {suggestion}")));
    }

    if lowered.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(CliError::user(format!(
            "`{trimmed}` starts with a digit; a crate name cannot"
        ))
        .with_help(format!("moso new app-{lowered}")));
    }

    let identifier = lowered.replace('-', "_");
    if KEYWORDS.contains(&identifier.as_str()) {
        return Err(CliError::user(format!(
            "`{trimmed}` is a Rust keyword and cannot name a crate"
        ))
        .with_help(format!("moso new {lowered}-api")));
    }
    if RESERVED.contains(&lowered.as_str()) {
        return Err(CliError::user(format!(
            "`{trimmed}` is reserved by Cargo and cannot name a crate"
        ))
        .with_help(format!("moso new {lowered}-api")));
    }

    Ok(lowered)
}

/// Turn something that is nearly a crate name into one.
fn suggest(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character);
            last_was_dash = false;
        } else if !last_was_dash && !out.is_empty() {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "my-api".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars::for_name("shop").expect("a valid name")
    }

    /// A key of the right shape and no secrecy: 32 bytes of `A`, base64.
    const FAKE_SECRET: &str = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQT0=";

    #[test]
    fn every_placeholder_is_substituted_in_every_file() {
        // Every variant, because a placeholder only one of them expands is a
        // literal `@@..@@` shipped into somebody's project.
        for vars in [
            vars(),
            vars().with_database(),
            vars().with_auth(FAKE_SECRET),
            vars().with_database().with_auth(FAKE_SECRET),
        ] {
            for (path, contents) in vars.render_all() {
                assert!(
                    !contents.contains("@@"),
                    "{} still contains a placeholder",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn the_template_only_uses_placeholders_the_renderer_knows() {
        let known = [
            "@@CRATE_NAME@@",
            "@@LIB_NAME@@",
            "@@ENV_PREFIX@@",
            "@@MOSO_DEP@@",
            "@@WORKSPACE@@",
            "@@DB_DEPS@@",
            "@@DB_MOD@@",
            "@@DB_CONFIG@@",
            "@@DB_DISPATCH@@",
            "@@DB_ENV@@",
            "@@AUTH_DEPS@@",
            "@@AUTH_MOD@@",
            "@@AUTH_CONFIG@@",
            "@@AUTH_SETUP@@",
            "@@AUTH_WIRING@@",
            "@@AUTH_ENV@@",
            "@@AUTH_DUMP@@",
            "@@SESSION_SECRET@@",
        ];
        // Every set: a `--with-db` or `--auth` file with a typo'd placeholder
        // would otherwise ship the literal `@@..@@` into a generated project.
        for file in FILES.iter().chain(DB_FILES).chain(AUTH_FILES) {
            let mut rest = file.contents;
            while let Some(start) = rest.find("@@") {
                let tail = &rest[start..];
                let end = tail[2..]
                    .find("@@")
                    .map(|offset| offset + 4)
                    .unwrap_or_else(|| panic!("{}: unterminated `@@`", file.path));
                let placeholder = &tail[..end];
                assert!(
                    known.contains(&placeholder),
                    "{}: unknown placeholder {placeholder}",
                    file.path
                );
                rest = &tail[end..];
            }
        }
    }

    #[test]
    fn names_are_lowercased_and_the_prefix_follows() {
        let vars = Vars::for_name("My-Shop").expect("valid");
        assert_eq!(vars.crate_name, "my-shop");
        assert_eq!(vars.lib_name, "my_shop");
        assert_eq!(vars.env_prefix, "MY_SHOP");
    }

    #[test]
    fn a_hyphenated_name_produces_an_importable_lib_name() {
        let vars = Vars::for_name("my-shop").expect("valid");
        let main = vars.render(
            FILES
                .iter()
                .find(|f| f.path == "src/main.rs")
                .expect("main")
                .contents,
        );
        assert!(main.contains("use my_shop::"), "{main}");
    }

    #[test]
    fn a_bad_name_is_a_user_error_with_a_paste_able_fix() {
        let error = Vars::for_name("my shop!").expect_err("rejected");
        assert_eq!(error.fault, crate::exit::Fault::User);
        assert_eq!(error.help.as_deref(), Some("moso new my-shop"));
    }

    #[test]
    fn a_keyword_and_a_reserved_name_are_both_refused() {
        assert!(validate_name("crate").is_err());
        assert!(validate_name("test").is_err());
        assert!(validate_name("9lives").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn a_path_dependency_replaces_the_published_one() {
        let vars = vars().with_moso_path(Path::new("../moso/crates/moso"));
        let manifest = vars.render(FILES[0].contents);
        assert!(
            manifest.contains(
                "moso = { path = \"../moso/crates/moso\", features = [\"config-file\"] }"
            ),
            "{manifest}"
        );
        assert!(!manifest.contains("moso = \"0."), "{manifest}");
    }

    #[test]
    fn the_published_dependency_tracks_the_cli_minor_version() {
        let expected = env!("CARGO_PKG_VERSION")
            .split('.')
            .take(2)
            .collect::<Vec<_>>()
            .join(".");
        assert_eq!(
            published_dependency(false),
            format!("moso = {{ version = \"{expected}\", features = [\"config-file\"] }}")
        );
        assert!(published_dependency(true).contains("\"auth\""));
        assert!(published_dependency(true).contains("\"config-file\""));
    }

    #[test]
    fn the_auth_feature_survives_a_path_dependency_in_either_order() {
        // `--moso-path` and `--auth` both write the `moso = ..` line, and the
        // command applies them in whichever order the flags were declared.
        let first = vars()
            .with_moso_path(Path::new("../moso/crates/moso"))
            .with_auth(FAKE_SECRET);
        let second = vars()
            .with_auth(FAKE_SECRET)
            .with_moso_path(Path::new("../moso/crates/moso"));

        for vars in [first, second] {
            let manifest = vars.render(FILES[0].contents);
            assert!(
                manifest
                    .contains("moso = { path = \"../moso/crates/moso\", features = [\"auth\", \"config-file\"] }"),
                "{manifest}"
            );
            // The kv crate is a sibling of `crates/moso`, not a published one.
            assert!(
                manifest.contains("moso-kv = { path = \"../moso/crates/moso-kv\" }"),
                "{manifest}"
            );
        }
    }

    #[test]
    fn the_auth_variant_writes_its_own_files_and_leaves_the_default_alone() {
        let plain: Vec<String> = vars()
            .render_all()
            .into_iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        assert!(!plain.iter().any(|path| path == "src/auth.rs"), "{plain:?}");
        assert!(!plain.iter().any(|path| path == ".env"), "{plain:?}");

        let with_auth: Vec<String> = vars()
            .with_auth(FAKE_SECRET)
            .render_all()
            .into_iter()
            .map(|(path, _)| path.display().to_string())
            .collect();
        for expected in ["src/auth.rs", "tests/auth.rs", ".env"] {
            assert!(
                with_auth.iter().any(|path| path == expected),
                "{expected} missing from {with_auth:?}"
            );
        }
    }

    #[test]
    fn the_generated_env_carries_the_key_it_was_given_and_nothing_else() {
        let rendered = vars().with_auth(FAKE_SECRET).render(
            AUTH_FILES
                .iter()
                .find(|file| file.path == ".env")
                .expect("the auth variant ships a .env")
                .contents,
        );
        assert!(
            rendered.contains(&format!("SHOP__SESSION_SECRET=base64:{FAKE_SECRET}")),
            "{rendered}"
        );
        // One key, so `moso config --check` cannot report a key nothing reads.
        assert_eq!(
            rendered
                .lines()
                .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
                .count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn the_auth_env_example_matches_the_shape_derive_config_renders() {
        let example = vars().with_auth(FAKE_SECRET).render(
            FILES
                .iter()
                .find(|file| file.path == ".env.example")
                .expect("env example")
                .contents,
        );
        // A secret is rendered with no value, and a required field carries the
        // marker on the last line of its doc comment.
        assert!(
            example.contains("at least 32 bytes.  [required]\nSHOP__SESSION_SECRET=\n"),
            "{example}"
        );
        assert!(example.contains("SHOP__HASH_MEMORY_KIB=19456"), "{example}");
        assert!(example.contains("SHOP__HASH_ITERATIONS=2"), "{example}");
        assert!(example.contains("SHOP__HASH_PARALLELISM=1"), "{example}");
    }

    #[test]
    fn the_auth_variant_answers_the_calibration_flag_and_the_default_refuses() {
        let dump = FILES
            .iter()
            .find(|file| file.path == "src/dump.rs")
            .expect("dump")
            .contents;

        let plain = vars().render(dump);
        assert!(plain.contains("moso new --auth"), "{plain}");
        assert!(!plain.contains("crate::auth::calibrate"), "{plain}");

        let with_auth = vars().with_auth(FAKE_SECRET).render(dump);
        assert!(
            with_auth.contains("crate::auth::calibrate(request).await"),
            "{with_auth}"
        );
    }

    #[test]
    fn the_workspace_stanza_is_absent_unless_asked_for() {
        assert!(!vars().render(FILES[0].contents).contains("[workspace]"));
        assert!(
            vars()
                .detached_from_workspace()
                .render(FILES[0].contents)
                .contains("[workspace]")
        );
    }

    #[test]
    fn the_generated_manifest_is_valid_toml() {
        let manifest = vars().detached_from_workspace().render(FILES[0].contents);
        let parsed: toml::Value = toml::from_str(&manifest).expect("valid TOML");
        assert_eq!(parsed["package"]["name"].as_str(), Some("shop"));
        assert!(parsed.get("workspace").is_some());
    }

    #[test]
    fn the_generated_cargo_config_is_valid_toml() {
        let config = vars().render(
            FILES
                .iter()
                .find(|f| f.path == ".cargo/config.toml")
                .expect("cargo config")
                .contents,
        );
        toml::from_str::<toml::Value>(&config).expect("valid TOML");
    }

    #[test]
    fn the_env_example_uses_the_projects_prefix() {
        let example = vars().render(
            FILES
                .iter()
                .find(|f| f.path == ".env.example")
                .expect("env example")
                .contents,
        );
        assert!(example.contains("SHOP__GREETING=hello"), "{example}");
    }
}
