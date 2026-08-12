//! The command line, as clap sees it.
//!
//! Kept in one file and free of logic so that `moso self completions` — which
//! is `clap_complete` walking this tree — cannot disagree with what the CLI
//! actually accepts.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::ui::ColorChoice;

/// The `moso` command line interface.
#[derive(Debug, Parser)]
#[command(
    name = "moso",
    version,
    about = "The Moso command line: scaffold a project, work in it, and ask it what it does.",
    long_about = None,
    propagate_version = true,
    disable_help_subcommand = true,
    after_help = "Exit codes: 0 ok, 1 user error, 2 usage error, 3 environment problem."
)]
pub struct Cli {
    /// Flags every subcommand accepts.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The flags shared by every subcommand.
///
/// Given their own help heading: they are `global`, so clap offers them under
/// every subcommand, and interleaving them with a subcommand's own flags makes
/// both harder to scan.
#[derive(Debug, Clone, Args)]
#[command(next_help_heading = "Global options")]
pub struct GlobalArgs {
    /// Print a JSON document instead of prose.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print only what was asked for.
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Print what is being done as it happens.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// When to colour the output. `NO_COLOR` overrides `auto`.
    #[arg(
        long,
        global = true,
        value_name = "WHEN",
        value_enum,
        default_value_t = ColorChoice::Auto
    )]
    pub color: ColorChoice,
}

/// The subcommands this build implements.
///
/// Declaration order is the order `moso --help` lists them, and it is a
/// lifecycle rather than an alphabet: create a project, work in it, verify it,
/// move its data, ask it questions, publish its contract, ship it. A reader
/// scanning the list for "what do I do next" finds the answer below where they
/// are rather than three screens away.
#[derive(Debug, Subcommand)]
pub enum Command {
    // ── create ──────────────────────────────────────────────────────────────
    /// Create a new Moso project.
    #[command(after_help = "\
Example:
  moso new shop --yes
  cd shop && cargo test")]
    New(NewArgs),

    /// Scaffold a resource into the project you are standing in.
    #[command(
        visible_alias = "g",
        after_help = "\
Examples:
  moso generate endpoint post       # src/posts.rs, mounted, with five routes
  moso generate schema invoice      # the payload types alone
  moso generate error billing       # an RFC 9457 taxonomy
  moso generate middleware observe  # a Layer/Service pair
  moso generate test posts          # an end-to-end contract test
  moso generate workspace           # split the crate into a Cargo workspace

Everything it writes is ordinary code that you own. Nothing is regenerated."
    )]
    Generate(GenerateArgs),

    // ── work ────────────────────────────────────────────────────────────────
    /// Rebuild and restart the application whenever a source file changes.
    #[command(after_help = "\
Example:
  moso dev
  moso dev --watch src --watch assets -- --port 8080

The server keeps running when a build fails, so a broken intermediate edit
costs you the compiler's message and nothing else. The application owns
standard output, so --json has nothing of its own to print.")]
    Dev(DevArgs),

    /// Build the application and run it once.
    #[command(after_help = "\
Examples:
  moso run
  moso run --release --profile production
  moso run -- --port 8080

Builds with cargo, runs the binary with the project root as its working
directory, and exits with whatever the application exited with. The
application owns standard output, so --json has nothing of its own to print.")]
    Run(RunArgs),

    // ── verify ──────────────────────────────────────────────────────────────
    /// Run the project's tests.
    #[command(after_help = "\
Examples:
  moso test                   # nextest when it is installed, cargo test otherwise
  moso test users             # only tests whose name contains `users`
  moso test -- --nocapture

Doctests run as a second pass, because no runner but `cargo test` can run them.
Suites that need DATABASE_URL or REDIS_URL skip when it is unset, and this
command says which ones rather than letting a green run hide them.")]
    Test(TestArgs),

    /// Report the mistakes rustc cannot see.
    #[command(after_help = "\
Examples:
  moso check                      # every lint that needs no battery
  moso check --authz              # and the authorization lints
  moso check --strict --json      # for CI

Exit code 1 when a lint at `deny` fires. `--strict` promotes warnings, and
`[lints]` in moso.toml sets the level of any lint by name.")]
    Check(CheckArgs),

    // ── data ────────────────────────────────────────────────────────────────
    /// Inspect and apply database migrations.
    #[command(after_help = "\
Examples:
  moso db status                  # what is applied, what is pending
  moso db migrate                 # apply everything pending
  moso db migrate --all-tenants   # …to every tenant your app lists
  moso db rollback --steps 2      # revert the last two
  moso db redo                    # revert one and apply it again
  moso db make-migration add_locale_to_users --dry-run
  moso db check                   # exits 1 on drift, so CI can gate on it
  moso db squash --yes            # collapse the applied history
  moso db seed dev                # insert the fixture data

Needs a project created with `moso new --with-db`: the commands run your
binary with a `--db-*` flag, and `src/db.rs` is what answers.")]
    Db {
        /// Which database operation to perform.
        #[command(subcommand)]
        command: DbCommand,
    },

    // ── ask the application ─────────────────────────────────────────────────
    /// List the routes the application registers.
    #[command(after_help = "\
Runs your binary with `--dump-routes` and renders what it answers. See
`src/dump.rs` in any project created by `moso new`.")]
    Routes(RoutesArgs),

    /// Show the composed middleware stack, global and per route.
    #[command(after_help = "\
Examples:
  moso middleware                 # the global stack, outermost first
  moso middleware --all           # disabled slots too
  moso middleware --route /users  # the effective stack for one route

`.layer()` and `.guard()` apply to the routes registered *before* the call, so
the per-route table is the answer to \"is this route actually covered\".")]
    Middleware(MiddlewareArgs),

    /// Show the resolved configuration, or regenerate `.env.example`.
    #[command(after_help = "\
Examples:
  moso config                       # every key, its value, and where it came from
  moso config --check               # in CI: fails on a typo, a drift, a leaked secret
  moso config --env-example --out .env.example
  moso config --generate-secret     # 32 bytes from the OS CSPRNG, base64

A generated secret is printed to standard output and nowhere else. Paste it
into `.env` or your platform's secret store; a `SecretBytes` field wants it
prefixed, as `base64:…` or `hex:…`.")]
    Config(ConfigArgs),

    /// Inspect the background queues.
    #[command(after_help = "\
Examples:
  moso jobs list                  # the registered job types
  moso jobs status                # depth, in flight, failures, latency
  moso jobs schedules             # the cron table with next occurrence
  moso jobs dlq --job send_mail   # page through the dead letters
  moso jobs retry --job send_mail --limit 50

Runs your binary with `--dump-jobs` and renders what it answers. A project
that does not use `moso-jobs` says so rather than printing an empty table.")]
    Jobs {
        /// Which queue operation to perform.
        #[command(subcommand)]
        command: JobsCommand,
    },

    /// Measure this machine's password hashing, and say what to configure.
    #[command(after_help = "\
Examples:
  moso auth calibrate --release       # ~250 ms per hash, on this machine
  moso auth calibrate --target-ms 500 # a slower login, a costlier crack

It runs your binary with `--dump-auth`, because argon2id parameters are a
property of the hardware the hash will run on: what takes 250 ms on a laptop
takes three times that in a container with half a CPU. It refuses to print
anything below OWASP's minimum.

Pass --release. An unoptimised argon2 is several times slower, so a debug build
reaches the target with parameters that are several times too weak for the
binary you will deploy.")]
    Auth {
        /// Which authentication question to ask.
        #[command(subcommand)]
        command: AuthCommand,
    },

    /// Inspect permissions and roles, and explain one decision.
    #[command(after_help = "\
Examples:
  moso authz permissions          # the registry, with its fingerprint
  moso authz roles                # each role and what it grants
  moso authz explain --actor usr_1 --action publish --resource Post#7

`explain` is refused in the production profile: the trace describes the whole
authorization model. Pass --allow-production when you mean it.")]
    Authz {
        /// Which authorization question to ask.
        #[command(subcommand)]
        command: AuthzCommand,
    },

    // ── the contract ────────────────────────────────────────────────────────
    /// Export, check and inspect the OpenAPI document.
    Openapi {
        /// Which OpenAPI operation to perform.
        #[command(subcommand)]
        command: OpenapiCommand,
    },

    /// Generate a typed client from the OpenAPI document.
    #[command(after_help = "\
Examples:
  moso client --out ../web/src/api                    # TypeScript, from your app
  moso client --lang rust --out ../sdk/src/api        # Rust, transport-agnostic
  moso client --input openapi.json --out src/api      # from a committed document
  moso client --out ../web/src/api --check            # in CI

The output is deterministic, so commit it and let --check fail the build when
it drifts from the contract.")]
    Client(ClientArgs),

    // ── ship ────────────────────────────────────────────────────────────────
    /// Build the application for deployment and report the artefact.
    #[command(after_help = "\
Examples:
  moso build                  # a release build, then the path and the size
  moso build --openapi        # and the contract, written beside the binary
  moso build --debug          # cargo's dev profile, for a quick smoke test")]
    Build(BuildArgs),

    /// Check this project against what a production deployment needs.
    #[command(after_help = "\
Example:
  moso deploy checklist       # exits non-zero on any failed check

It deploys nothing and writes nothing. It reads the configuration your
application resolves under the production profile, plus the project on disk,
and reports what would be wrong once it is deployed.")]
    Deploy {
        /// Which deployment operation to perform.
        #[command(subcommand)]
        command: DeployCommand,
    },

    // ── this machine, and this binary ───────────────────────────────────────
    /// Check that this machine can build and run a Moso project.
    Doctor(DoctorArgs),

    /// Commands about the CLI itself.
    #[command(name = "self")]
    Own {
        /// Which self-management operation to perform.
        #[command(subcommand)]
        command: SelfCommand,
    },
}

/// `moso new`.
#[derive(Debug, Clone, Args)]
pub struct NewArgs {
    /// The project's name. Becomes the crate name and the directory.
    pub name: String,

    /// Create it here instead of `./<name>`.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Accept every default without asking.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Do not initialise a git repository.
    #[arg(long)]
    pub no_git: bool,

    /// Write into a directory that already has files in it.
    #[arg(long)]
    pub force: bool,

    /// Depend on a Moso checkout on disk instead of the published crate.
    ///
    /// The path is written into the generated `Cargo.toml` verbatim, so a
    /// relative path stays relative.
    #[arg(long, value_name = "DIR")]
    pub moso_path: Option<PathBuf>,

    /// Add the migration story: `migrations/`, `src/db.rs` and `moso db`.
    ///
    /// Off by default because it pulls a database driver, and an application
    /// that does not need one should not compile sqlx to find that out.
    #[arg(long)]
    pub with_db: bool,

    /// Copy the authentication flows into the project: `src/auth.rs`.
    ///
    /// Registration, login, logout, the session listing and password reset,
    /// over a user type declared in your crate, with `#[endpoint]` on every
    /// handler so they are in your own OpenAPI document. The mounted
    /// `moso::auth::routes()` cannot be, and is fixed to the framework's user
    /// type — which is why this tier exists.
    ///
    /// Also writes a `.env` holding a session signing key generated from this
    /// machine's random number generator. It is gitignored.
    #[arg(long)]
    pub auth: bool,
}

/// The flags every command that interrogates the application accepts.
///
/// Not `global`: it is flattened into several subcommands, and a global
/// argument may only be declared once in a command tree.
#[derive(Debug, Clone, Args, Default)]
pub struct AppArgs {
    /// The `Cargo.toml` of the package to interrogate.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,

    /// Which binary to run, for a package that has more than one.
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Build with `--release`.
    #[arg(long)]
    pub release: bool,

    /// Cargo features to enable, comma separated.
    #[arg(long, value_name = "FEATURES")]
    pub features: Option<String>,
}

/// `moso openapi <sub>`.
#[derive(Debug, Subcommand)]
pub enum OpenapiCommand {
    /// Write the OpenAPI document.
    #[command(after_help = "\
Examples:
  moso openapi export --out openapi.json
  moso openapi export --prefix /api/v1 --out openapi.v1.json  # one version's slice
  moso openapi check                        # in CI")]
    Export(OpenapiExportArgs),

    /// Fail if the committed document is out of date.
    Check(OpenapiCheckArgs),
}

/// `moso openapi export`.
#[derive(Debug, Clone, Args)]
pub struct OpenapiExportArgs {
    /// Where to write it. Standard output when absent.
    #[arg(long, short, value_name = "PATH")]
    pub out: Option<PathBuf>,

    /// Indent the JSON. This is the default; the flag is for scripts that
    /// prefer to say so.
    #[arg(long)]
    pub pretty: bool,

    /// Emit the JSON on one line instead.
    #[arg(long, conflicts_with = "pretty")]
    pub compact: bool,

    /// Keep only the paths at or under this prefix, dropping every other
    /// operation.
    ///
    /// The match is on segment boundaries, so `--prefix /api` keeps `/api` and
    /// `/api/v1/users` but never `/apiary`. It filters `paths` alone and leaves
    /// `components/schemas` whole — deciding which schema a kept path still
    /// needs is transitive `$ref` tracing, and an over-broad component set is
    /// valid OpenAPI while a schema pruned by mistake is a broken document.
    /// This is how a multi-version API is split into one document per version.
    #[arg(long, value_name = "PATH")]
    pub prefix: Option<String>,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso openapi check`.
#[derive(Debug, Clone, Args)]
pub struct OpenapiCheckArgs {
    /// The committed document to compare against.
    #[arg(default_value = "openapi.json", value_name = "PATH")]
    pub path: PathBuf,

    /// Fail only on a *breaking* change rather than on any drift.
    ///
    /// The default `check` treats every difference as a reason to fail — the
    /// committed document is a byte-for-meaning contract and any edit to the
    /// code must be committed with it. `--breaking` classifies each difference
    /// instead: an additive change (a new endpoint, a new optional field, a new
    /// error status) passes, and only a change an existing correct client can
    /// observe as a regression (a removed path, a removed success response, a
    /// narrowed type, a new required request field, a dropped enum value) fails.
    #[arg(long)]
    pub breaking: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// What `moso generate` knows how to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GenerateKind {
    /// A resource module: payloads, a store, five handlers and a router.
    Endpoint,
    /// The payload types for a resource, without the handlers.
    Schema,
    /// An RFC 9457 error taxonomy for one domain.
    Error,
    /// A `tower` layer built from one `async fn`.
    Middleware,
    /// An end-to-end test that boots the real application.
    Test,
    /// The Cargo workspace split of `00-foundations/04-project-structure.md`.
    ///
    /// The one kind that takes no name: it operates on the project itself
    /// rather than writing a resource into it.
    Workspace,
}

/// `moso generate`.
#[derive(Debug, Clone, Args)]
pub struct GenerateArgs {
    /// What to write.
    #[arg(value_enum)]
    pub kind: GenerateKind,

    /// What to call it. `post`, `posts` and `BlogPost` are all accepted.
    ///
    /// Optional here and required by every kind but `workspace`, which
    /// restructures the project it is standing in and has nothing to name.
    /// Clap can express "required when another argument equals a value" but not
    /// its negation, and spelling the rule as a list of the five kinds that do
    /// need a name is a list that goes stale the day a sixth is added — so the
    /// check lives next to the kinds themselves, in
    /// [`commands::generate`](crate::commands::generate).
    pub name: Option<String>,

    /// The singular form, when the guess would be wrong.
    ///
    /// `moso generate endpoint geese --singular goose`.
    #[arg(long, value_name = "WORD")]
    pub singular: Option<String>,

    /// Print what would be written without writing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Overwrite a file that already exists.
    #[arg(long)]
    pub force: bool,

    /// The `Cargo.toml` of the package to write into.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
}

/// `moso dev`.
#[derive(Debug, Clone, Args)]
pub struct DevArgs {
    /// Watch this path instead of the defaults. Repeatable.
    ///
    /// Without it: `src`, `Cargo.toml`, `Cargo.lock`, `build.rs`, `config`,
    /// `templates`, `migrations` and `.env`, whichever exist.
    #[arg(long, value_name = "PATH")]
    pub watch: Vec<PathBuf>,

    /// How often to look for changes, in milliseconds.
    #[arg(long, value_name = "MS", default_value_t = 300, value_parser = clap::value_parser!(u64).range(20..=60_000))]
    pub poll: u64,

    /// Stop instead of waiting for the next change when a build fails.
    ///
    /// For a CI or agent-driven loop that wants a non-zero exit rather than a
    /// process that stays up.
    #[arg(long)]
    pub exit_on_error: bool,

    /// How to build the application.
    #[command(flatten)]
    pub app: AppArgs,

    /// Arguments passed through to the application itself.
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

/// `moso db <sub>`.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Report which migrations are applied, pending or in trouble.
    Status(DbArgs),

    /// Apply every pending migration.
    #[command(visible_alias = "up")]
    Migrate(DbMigrateArgs),

    /// Revert the most recently applied migrations.
    #[command(visible_alias = "down")]
    Rollback(DbRollbackArgs),

    /// Revert one migration and apply it again.
    ///
    /// The edit loop for a migration you are still writing.
    Redo(DbArgs),

    /// Write a migration from the difference between your entities and the
    /// committed snapshot.
    #[command(
        name = "make-migration",
        visible_alias = "make",
        after_help = "\
Nothing here touches the database: the diff is against `migrations/.schema.json`,
so the same entities produce the same bytes on every machine.

A rename cannot be told from a drop and an add by looking at a diff, and the
difference is whether the column's data survives — so an unanswered rename is
refused rather than guessed:

  moso db make-migration rename_name --rename name:full_name
  moso db make-migration reset_tags --drop-and-add"
    )]
    MakeMigration(DbMakeMigrationArgs),

    /// Report where the entity graph, the migration files and the database
    /// disagree.
    #[command(after_help = "\
Exits 1 when the live database does not match your entities, in either
direction — a migration nobody applied, or a column somebody added by hand in
psql. Pending migrations alone are reported and do not fail it: a pending
migration is the fix, not a second problem.")]
    Check(DbArgs),

    /// Collapse every migration into one baseline.
    #[command(after_help = "\
Prints what it would do and writes nothing. `--yes` writes the baseline and
deletes the files it replaces.

Refused unless every migration on disk is already applied and the ledger is
sound: the baseline carries `-- moso:replaces`, and a database that has not run
one of the files it names would run the whole baseline over a schema it already
half has.")]
    Squash(DbSquashArgs),

    /// Run the project's seeds.
    #[command(after_help = "\
Seeds are fixture data, not migrations: not versioned, not recorded, and meant
to be run again. Under a production profile every seed is refused unless it
declares itself safe there or `--force` is typed.")]
    Seed(DbSeedArgs),
}

/// The flags every `moso db` subcommand accepts.
#[derive(Debug, Clone, Args, Default)]
pub struct DbArgs {
    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso db migrate`.
#[derive(Debug, Clone, Args, Default)]
pub struct DbMigrateArgs {
    /// Migrate every tenant the application lists instead of one database.
    ///
    /// The list is `tenants()` in `src/db.rs`, because Moso does not know where
    /// you keep it. A tenant that fails does not stop the rest, and the command
    /// exits non-zero naming the ones that did.
    #[arg(long)]
    pub all_tenants: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso db make-migration`.
#[derive(Debug, Clone, Args)]
pub struct DbMakeMigrationArgs {
    /// What the migration is called: `add_locale_to_users`.
    ///
    /// Slugified and prefixed with a UTC timestamp, so the file lands as
    /// `migrations/YYYYMMDDTHHMMSS_add_locale_to_users.sql`.
    pub name: String,

    /// Print the migration and write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Answer one rename question: `--rename old_name:new_name`. Repeatable.
    #[arg(long, value_name = "OLD:NEW")]
    pub rename: Vec<String>,

    /// Treat every rename question `--rename` did not answer as a drop and an
    /// add.
    ///
    /// The data in those columns does not survive. Reasonable against an empty
    /// database and almost never otherwise.
    #[arg(long)]
    pub drop_and_add: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso db squash`.
#[derive(Debug, Clone, Args)]
pub struct DbSquashArgs {
    /// Write the baseline and delete the migrations it replaces.
    ///
    /// Without it the command is a report: a squash rewrites version-controlled
    /// history and deletes files, so the destructive half has to be typed.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso db seed`.
#[derive(Debug, Clone, Args)]
pub struct DbSeedArgs {
    /// Which seed to run. Every registered seed when absent.
    pub name: Option<String>,

    /// Seed a production profile anyway.
    #[arg(long)]
    pub force: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso db rollback`.
#[derive(Debug, Clone, Args)]
pub struct DbRollbackArgs {
    /// How many migrations to revert.
    ///
    /// One by default: reverting more than you meant to is the mistake this
    /// command can make, so it is the flag that has to be typed.
    #[arg(long, short, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(usize))]
    pub steps: usize,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso routes`.
#[derive(Debug, Clone, Args)]
pub struct RoutesArgs {
    /// Show only the routes carrying this tag.
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Show the routes hidden from the OpenAPI document too.
    #[arg(long)]
    pub all: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// The languages `moso client` generates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ClientLang {
    /// TypeScript over `fetch`, with no runtime dependency at all.
    Ts,
    /// Rust over `serde`, with the HTTP client left to you.
    Rust,
}

/// `moso client`.
#[derive(Debug, Clone, Args)]
pub struct ClientArgs {
    /// Which language to generate.
    #[arg(long, value_enum, default_value = "ts")]
    pub lang: ClientLang,

    /// The directory to write into. Created if it does not exist.
    #[arg(long, short, value_name = "DIR")]
    pub out: PathBuf,

    /// Read the document from this file instead of running the application.
    ///
    /// With it, no Rust project is needed: the command works in a front-end
    /// repository that has only the committed document.
    #[arg(long, short, value_name = "PATH")]
    pub input: Option<PathBuf>,

    /// Fail if what is on disk differs from what would be generated.
    #[arg(long)]
    pub check: bool,

    /// How to reach the application. Ignored when `--input` is given.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso middleware`.
#[derive(Debug, Clone, Args)]
pub struct MiddlewareArgs {
    /// Show the effective stack for the routes matching this path.
    #[arg(long, value_name = "PATH")]
    pub route: Option<String>,

    /// Show the slots that are present but disabled too.
    #[arg(long)]
    pub all: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso check`.
#[derive(Debug, Clone, Args)]
pub struct CheckArgs {
    /// Also run the lints that need the authorization battery.
    ///
    /// Separate because they ask the application a question only a project
    /// using `moso-authz` can answer, and a project that does not use it should
    /// not be told it failed a check it cannot run.
    #[arg(long)]
    pub authz: bool,

    /// Treat every warning as an error.
    #[arg(long)]
    pub strict: bool,

    /// Run only this lint. Repeatable.
    #[arg(long, value_name = "NAME")]
    pub lint: Vec<String>,

    /// Print the lints, their default levels and what they catch, then stop.
    #[arg(long, conflicts_with_all = ["authz", "strict"])]
    pub list: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso jobs <sub>`.
#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// List the job types the application registers.
    List(JobsArgs),

    /// Report queue depth, in-flight work, retries and failures.
    Status(JobsArgs),

    /// Show the schedule table with each entry's next occurrence.
    Schedules(JobsArgs),

    /// Page through the dead-letter queue.
    Dlq(JobsDlqArgs),

    /// Move dead letters back onto their queues.
    Retry(JobsBulkArgs),

    /// Delete dead letters without running them.
    #[command(after_help = "\
This throws work away. It asks first unless --yes is given.")]
    Discard(JobsBulkArgs),
}

/// The flags every `moso jobs` subcommand accepts.
#[derive(Debug, Clone, Args, Default)]
pub struct JobsArgs {
    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// Which dead letters a `moso jobs` subcommand acts on.
///
/// Flattened into three subcommands rather than repeated in each: the filter a
/// person used to *look* at a page is the filter they then want to retry, and
/// two spellings of one concept is how the two drift apart.
#[derive(Debug, Clone, Args, Default)]
pub struct DlqFilterArgs {
    /// Only this job's wire name.
    #[arg(long, value_name = "NAME")]
    pub job: Option<String>,

    /// Only this queue.
    #[arg(long, value_name = "NAME")]
    pub queue: Option<String>,

    /// Only failures whose error chain contains this text.
    #[arg(long, value_name = "TEXT")]
    pub error: Option<String>,

    /// Exactly this dead letter, ignoring the other filters.
    #[arg(long, value_name = "ID", conflicts_with_all = ["job", "queue", "error"])]
    pub id: Option<String>,
}

/// `moso jobs dlq`.
#[derive(Debug, Clone, Args)]
pub struct JobsDlqArgs {
    /// Which dead letters to show.
    #[command(flatten)]
    pub filter: DlqFilterArgs,

    /// How many to show.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: u32,

    /// Continue from the cursor the previous page printed.
    #[arg(long, value_name = "CURSOR")]
    pub cursor: Option<String>,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso jobs retry` and `moso jobs discard`.
#[derive(Debug, Clone, Args)]
pub struct JobsBulkArgs {
    /// Which dead letters to act on.
    #[command(flatten)]
    pub filter: DlqFilterArgs,

    /// How many at most.
    ///
    /// Mandatory in spirit and capped by default: a bulk operation over an
    /// unbounded filter is how a fix becomes an outage.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: u32,

    /// Do not ask before acting.
    ///
    /// Only `discard` asks — it throws work away, and `retry` puts it back —
    /// but the flag is accepted by both so that a script can pass it uniformly
    /// without knowing which of the two it is running.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso auth <sub>`.
///
/// One subcommand, and it will stay small: everything else about authentication
/// is a decision the application makes in its own composition root, and a CLI
/// that grew a `moso auth create-user` would be inventing a user model for
/// somebody who already has one.
#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Measure argon2id on this machine and print the parameters to configure.
    Calibrate(AuthCalibrateArgs),
}

/// `moso auth calibrate`.
#[derive(Debug, Clone, Args)]
pub struct AuthCalibrateArgs {
    /// How long one password hash should take, in milliseconds.
    ///
    /// 250 ms by default, which is `moso_auth::TARGET_HASH_TIME`: slow enough
    /// that offline cracking is expensive, fast enough that a login does not
    /// feel broken. The floor is 50 because anything faster is not a password
    /// hash, and the ceiling is 2000 because past it the login is the outage.
    #[arg(
        long,
        value_name = "MS",
        default_value_t = 250,
        value_parser = clap::value_parser!(u64).range(50..=2000)
    )]
    pub target_ms: u64,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso authz <sub>`.
#[derive(Debug, Subcommand)]
pub enum AuthzCommand {
    /// List every declared permission, with the registry's fingerprint.
    Permissions(AuthzPermissionsArgs),

    /// List every role and the permissions it grants.
    Roles(AuthzArgs),

    /// Say why one actor may or may not do one thing.
    Explain(AuthzExplainArgs),
}

/// `moso authz roles`.
#[derive(Debug, Clone, Args, Default)]
pub struct AuthzArgs {
    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso authz permissions`.
///
/// A struct of its own rather than one shared with `roles`, because `--group`
/// selects rows of the permission registry and there is no such column in the
/// role table. Sharing it offered `moso authz roles --group billing`, which
/// parsed, printed every role, and told nobody it had ignored the filter.
#[derive(Debug, Clone, Args, Default)]
pub struct AuthzPermissionsArgs {
    /// Show only the permissions in this group.
    #[arg(long, value_name = "NAME")]
    pub group: Option<String>,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso authz explain`.
#[derive(Debug, Clone, Args)]
pub struct AuthzExplainArgs {
    /// Who is asking.
    #[arg(long, value_name = "ID")]
    pub actor: String,

    /// What they are trying to do: a permission name or a policy action.
    #[arg(long, value_name = "NAME")]
    pub action: String,

    /// What they are trying to do it to, as `Entity#id`.
    #[arg(long, value_name = "RESOURCE")]
    pub resource: Option<String>,

    /// The scope to evaluate in. Global when absent.
    #[arg(long, value_name = "KEY")]
    pub scope: Option<String>,

    /// Produce the trace even in the production profile.
    ///
    /// Refused there by default, for the same reason the `X-Moso-Authz-Explain`
    /// header is: the trace describes the whole authorization model.
    #[arg(long)]
    pub allow_production: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso doctor`.
#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    /// The project to check. Without it, only the machine is checked.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
}

/// How `moso config --generate-secret` encodes the bytes it read.
///
/// The two spellings `SecretBytes` accepts, so the output can be pasted after
/// the `base64:` or `hex:` prefix that type requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SecretFormat {
    /// Standard base64 with padding — `base64:` in a `SecretBytes` field.
    #[default]
    Base64,
    /// Lower-case hexadecimal — `hex:` in a `SecretBytes` field.
    Hex,
}

impl SecretFormat {
    /// The name `--json` prints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SecretFormat::Base64 => "base64",
            SecretFormat::Hex => "hex",
        }
    }
}

/// `moso config`.
#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    /// Print a `.env.example` regenerated from the application's `Config` type.
    #[arg(long)]
    pub env_example: bool,

    /// Report configuration problems, and exit non-zero when there are any.
    ///
    /// Resolves the configuration exactly as the application does, then looks
    /// for the four mistakes that are silent at runtime: an environment key no
    /// field reads, a committed `.env.example` that has drifted from the
    /// `Config` type, a secret whose value came from a committed file, and a
    /// key nothing supplies.
    #[arg(long, conflicts_with_all = ["env_example", "generate_secret"])]
    pub check: bool,

    /// Print one new secret from the operating system's random number
    /// generator, and nothing else.
    ///
    /// Needs no project: it is entropy, not configuration.
    #[arg(long, conflicts_with = "env_example")]
    pub generate_secret: bool,

    /// How to encode a generated secret.
    #[arg(
        long,
        value_name = "FORMAT",
        value_enum,
        default_value_t = SecretFormat::Base64,
        requires = "generate_secret"
    )]
    pub format: SecretFormat,

    /// How many random bytes a generated secret carries.
    ///
    /// The floor is 16 because a key shorter than that is not a key, and the
    /// ceiling is 1024 because nothing that goes in an environment variable
    /// needs more.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 32,
        requires = "generate_secret",
        value_parser = clap::value_parser!(u32).range(16..=1024)
    )]
    pub bytes: u32,

    /// Write to this file instead of standard output.
    #[arg(long, short, value_name = "PATH", conflicts_with = "generate_secret")]
    pub out: Option<PathBuf>,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// Which set of defaults an application runs under.
///
/// Deliberately *not* cargo's build profile, which `--release` selects. This is
/// `MOSO_PROFILE`: it decides which `config/<profile>.toml` is read, whether
/// `.env` is loaded, and whether `/docs` is mounted. The two are independent —
/// a release build under the `dev` profile still serves its documentation UI —
/// and conflating them is how a debug-shaped application reaches production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    /// Local development: loads `.env`, mounts `/docs`, renders rich errors.
    Dev,
    /// Automated tests: loads `.env`, otherwise production-shaped.
    Test,
    /// Deployed: no `.env`, no error detail, no documentation UI.
    #[value(alias = "prod")]
    Production,
}

impl Profile {
    /// The spelling `MOSO_PROFILE` takes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::Dev => "dev",
            Profile::Test => "test",
            Profile::Production => "production",
        }
    }
}

/// `moso run`.
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// The profile to run under. Sets `MOSO_PROFILE` for the application.
    #[arg(long, value_name = "PROFILE", value_enum)]
    pub profile: Option<Profile>,

    /// How to build the application.
    #[command(flatten)]
    pub app: AppArgs,

    /// Arguments passed through to the application itself.
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

/// `moso build`.
///
/// Does not flatten [`AppArgs`]: this command builds for release by default, so
/// a `--release` that did nothing would be worse than no flag at all. `--debug`
/// is the opt-out.
#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    /// Build with cargo's dev profile instead of release.
    #[arg(long)]
    pub debug: bool,

    /// Export the OpenAPI document beside the binary.
    #[arg(long)]
    pub openapi: bool,

    /// Write the exported document here instead of beside the binary.
    #[arg(long, value_name = "PATH", requires = "openapi")]
    pub openapi_out: Option<PathBuf>,

    /// The `Cargo.toml` of the package to build.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,

    /// Which binary to build, for a package that has more than one.
    #[arg(long, value_name = "NAME")]
    pub bin: Option<String>,

    /// Cargo features to enable, comma separated.
    #[arg(long, value_name = "FEATURES")]
    pub features: Option<String>,
}

/// `moso test`.
#[derive(Debug, Clone, Args)]
pub struct TestArgs {
    /// Run only the tests whose name contains this.
    #[arg(value_name = "FILTER")]
    pub filter: Option<String>,

    /// Test every package in the workspace, not only the one found from here.
    ///
    /// For a project split by `moso generate workspace`, where the crate you are
    /// standing in is not the whole application.
    #[arg(long)]
    pub workspace: bool,

    /// Use `cargo test` even when `cargo-nextest` is installed.
    #[arg(long)]
    pub no_nextest: bool,

    /// Skip the doctest pass.
    #[arg(long)]
    pub no_doc: bool,

    /// Enable every feature the package declares.
    #[arg(long, conflicts_with = "features")]
    pub all_features: bool,

    /// Cargo features to enable, comma separated.
    #[arg(long, value_name = "FEATURES")]
    pub features: Option<String>,

    /// The `Cargo.toml` of the package to test.
    #[arg(long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,

    /// Arguments passed through to the test runner.
    #[arg(last = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

/// `moso deploy <sub>`.
///
/// One subcommand, and deliberately so: Moso is not a PaaS and this command
/// tree must never grow something that pushes an artefact anywhere.
#[derive(Debug, Subcommand)]
pub enum DeployCommand {
    /// Report what a production deployment of this project still needs.
    Checklist(DeployChecklistArgs),
}

/// `moso deploy checklist`.
#[derive(Debug, Clone, Args)]
pub struct DeployChecklistArgs {
    /// Resolve the configuration under this profile.
    ///
    /// `production` by default: auditing development values before a production
    /// deployment answers a question nobody asked.
    #[arg(long, value_name = "PROFILE", value_enum, default_value_t = Profile::Production)]
    pub profile: Profile,

    /// Fail on warnings too.
    #[arg(long)]
    pub strict: bool,

    /// How to reach the application.
    #[command(flatten)]
    pub app: AppArgs,
}

/// `moso self update`.
#[derive(Debug, Clone, Args)]
pub struct SelfUpdateArgs {
    /// Ask the crates registry which version is the latest published one.
    ///
    /// The only network access this CLI ever makes, and it happens only when
    /// this flag is given.
    #[arg(long)]
    pub check: bool,
}

/// `moso self <sub>`.
#[derive(Debug, Subcommand)]
pub enum SelfCommand {
    /// Print a shell completion script.
    #[command(after_help = "\
Examples:
  moso self completions zsh  > ~/.zfunc/_moso
  moso self completions bash > /etc/bash_completion.d/moso
  moso self completions fish > ~/.config/fish/completions/moso.fish")]
    Completions {
        /// Which shell to generate for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Report the running version and how to update it.
    #[command(after_help = "\
Examples:
  moso self update            # the running version and the command that updates it
  moso self update --check    # ask the registry what the latest version is

It never replaces this binary. Whatever installed it — cargo, a package
manager, an archive somebody unpacked — is what can correctly replace it, so
this command names that command instead of guessing at one.")]
    Update(SelfUpdateArgs),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_tree_is_internally_consistent() {
        // Catches duplicate flags, bad defaults and conflicting `conflicts_with`
        // at test time rather than at the user's first invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn every_subcommand_has_a_description() {
        for subcommand in Cli::command().get_subcommands() {
            assert!(
                subcommand.get_about().is_some(),
                "`{}` has no about text",
                subcommand.get_name()
            );
        }
    }

    #[test]
    fn json_is_accepted_after_every_subcommand() {
        let cli = Cli::try_parse_from(["moso", "routes", "--json"]).expect("parses");
        assert!(cli.global.json);
        let cli = Cli::try_parse_from(["moso", "doctor", "--json"]).expect("parses");
        assert!(cli.global.json);
    }

    #[test]
    fn new_takes_the_documented_flags() {
        let cli =
            Cli::try_parse_from(["moso", "new", "shop", "--yes", "--no-git"]).expect("parses");
        match cli.command {
            Command::New(args) => {
                assert_eq!(args.name, "shop");
                assert!(args.yes);
                assert!(args.no_git);
                assert!(!args.with_db);
                assert!(!args.auth, "both variants are off unless asked for");
            }
            other => panic!("parsed as {other:?}"),
        }

        // The two variants are independent: an application can want accounts
        // without migrations, and migrations without accounts.
        let cli = Cli::try_parse_from(["moso", "new", "shop", "--auth"]).expect("parses");
        match cli.command {
            Command::New(args) => {
                assert!(args.auth);
                assert!(!args.with_db);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn calibrate_defaults_to_the_target_the_battery_documents() {
        let cli = Cli::try_parse_from(["moso", "auth", "calibrate"]).expect("parses");
        match cli.command {
            Command::Auth {
                command: AuthCommand::Calibrate(args),
            } => assert_eq!(args.target_ms, 250),
            other => panic!("parsed as {other:?}"),
        }

        // A target outside the band is refused rather than measured: below 50 ms
        // it is not a password hash, and above two seconds the login is the
        // outage.
        assert!(Cli::try_parse_from(["moso", "auth", "calibrate", "--target-ms", "5"]).is_err());
        assert!(
            Cli::try_parse_from(["moso", "auth", "calibrate", "--target-ms", "60000"]).is_err()
        );
    }

    #[test]
    fn openapi_export_defaults_to_stdout_and_pretty() {
        let cli = Cli::try_parse_from(["moso", "openapi", "export"]).expect("parses");
        match cli.command {
            Command::Openapi {
                command: OpenapiCommand::Export(args),
            } => {
                assert!(args.out.is_none());
                assert!(!args.compact);
                assert!(args.prefix.is_none());
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn openapi_export_takes_a_prefix_to_slice_the_document() {
        let cli = Cli::try_parse_from(["moso", "openapi", "export", "--prefix", "/api/v1"])
            .expect("parses");
        match cli.command {
            Command::Openapi {
                command: OpenapiCommand::Export(args),
            } => assert_eq!(args.prefix.as_deref(), Some("/api/v1")),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn openapi_check_defaults_to_the_conventional_path() {
        let cli = Cli::try_parse_from(["moso", "openapi", "check"]).expect("parses");
        match cli.command {
            Command::Openapi {
                command: OpenapiCommand::Check(args),
            } => {
                assert_eq!(args.path, PathBuf::from("openapi.json"));
                assert!(!args.breaking);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn openapi_check_takes_a_breaking_flag() {
        let cli = Cli::try_parse_from(["moso", "openapi", "check", "--breaking"]).expect("parses");
        match cli.command {
            Command::Openapi {
                command: OpenapiCommand::Check(args),
            } => assert!(args.breaking),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn quiet_and_verbose_cannot_both_be_given() {
        assert!(Cli::try_parse_from(["moso", "doctor", "-q", "-v"]).is_err());
    }

    #[test]
    fn an_unknown_subcommand_is_a_parse_error() {
        assert!(Cli::try_parse_from(["moso", "provision"]).is_err());
    }

    #[test]
    fn a_command_group_without_its_subcommand_is_a_parse_error() {
        // Every one of these is a namespace rather than a command: `moso db`
        // alone has no meaning, and clap's error lists the subcommands that do.
        for group in ["db", "jobs", "auth", "authz", "openapi", "deploy", "self"] {
            assert!(
                Cli::try_parse_from(["moso", group]).is_err(),
                "`moso {group}` should require a subcommand"
            );
        }
    }

    #[test]
    fn group_filters_the_permission_registry_and_no_other_table() {
        let cli = Cli::try_parse_from(["moso", "authz", "permissions", "--group", "billing"])
            .expect("parses");
        match cli.command {
            Command::Authz {
                command: AuthzCommand::Permissions(args),
            } => assert_eq!(args.group.as_deref(), Some("billing")),
            other => panic!("parsed as {other:?}"),
        }
        // The role table has no group column, so the flag is refused rather
        // than accepted and dropped on the floor.
        assert!(Cli::try_parse_from(["moso", "authz", "roles", "--group", "billing"]).is_err());
    }

    #[test]
    fn the_help_lists_the_commands_in_lifecycle_order() {
        // Declaration order *is* the help order, so an alphabetical slip is a
        // test failure rather than something a reader notices later.
        let tree = Cli::command();
        let names: Vec<&str> = tree
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert_eq!(
            names,
            vec![
                "new",
                "generate",
                "dev",
                "run",
                "test",
                "check",
                "db",
                "routes",
                "middleware",
                "config",
                "jobs",
                "auth",
                "authz",
                "openapi",
                "client",
                "build",
                "deploy",
                "doctor",
                "self",
            ]
        );
    }

    #[test]
    fn generate_workspace_is_the_one_kind_that_takes_no_name() {
        let cli = Cli::try_parse_from(["moso", "generate", "workspace"]).expect("parses");
        match cli.command {
            Command::Generate(args) => {
                assert_eq!(args.kind, GenerateKind::Workspace);
                assert!(args.name.is_none());
            }
            other => panic!("parsed as {other:?}"),
        }
        // Every other kind still has to be told what to call the thing, but the
        // rule is not one clap can express — see `commands::generate`, which
        // turns a missing name into the same exit code clap would have.
        let cli = Cli::try_parse_from(["moso", "generate", "endpoint"]).expect("parses");
        match cli.command {
            Command::Generate(args) => assert!(args.name.is_none()),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_generated_secret_cannot_be_written_to_a_file() {
        // `--out` would put the secret somewhere it outlives the terminal, and
        // the most likely `somewhere` is inside the repository.
        assert!(
            Cli::try_parse_from(["moso", "config", "--generate-secret", "--out", "key.txt"])
                .is_err()
        );
        let cli = Cli::try_parse_from(["moso", "config", "--generate-secret", "--bytes", "64"])
            .expect("parses");
        match cli.command {
            Command::Config(args) => {
                assert!(args.generate_secret);
                assert_eq!(args.bytes, 64);
                assert_eq!(args.format, SecretFormat::Base64);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn check_is_a_mode_of_its_own_and_a_secret_is_at_least_sixteen_bytes() {
        assert!(Cli::try_parse_from(["moso", "config", "--check"]).is_ok());
        assert!(Cli::try_parse_from(["moso", "config", "--check", "--env-example"]).is_err());
        assert!(
            Cli::try_parse_from(["moso", "config", "--generate-secret", "--bytes", "8"]).is_err()
        );
    }

    #[test]
    fn the_self_subcommand_is_spelled_self() {
        let cli = Cli::try_parse_from(["moso", "self", "completions", "zsh"]).expect("parses");
        match cli.command {
            Command::Own {
                command: SelfCommand::Completions { shell },
            } => assert_eq!(shell, clap_complete::Shell::Zsh),
            other => panic!("parsed as {other:?}"),
        }
    }
}
