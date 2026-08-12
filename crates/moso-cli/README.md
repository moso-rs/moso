# moso-cli

**The `moso` command: project scaffolding, route listing, configuration
inspection and OpenAPI export.**

```sh
cargo install moso-cli
```

The binary is called `moso`; the package is `moso-cli` because
[`moso`](../moso) is the library an application depends on.

## Commands

| Command | What it does |
| --- | --- |
| `moso new <name>` | scaffold a project that builds, tests and serves `/docs` on the first run |
| `moso routes` | list the routes the application registers, with method, path and operation id |
| `moso openapi export` | write the OpenAPI document (`--out openapi.json`, or standard output) |
| `moso openapi check` | fail if the committed document is out of date — the CI drift gate |
| `moso config` | show the resolved configuration and where each value came from, or regenerate `.env.example` |
| `moso doctor` | check that this machine can build and run a Moso project |
| `moso self completions <shell>` | print a shell completion script |

```sh
moso new shop --yes
cd shop && cargo test
moso openapi export --out openapi.json
```

## How `routes` and `openapi` reach your application

They run your binary with `--dump-routes` / `--dump-openapi` and render what it
answers, rather than reimplementing the router. A project created by
`moso new` ships the `src/dump.rs` that handles those flags. This is why the
output is always the truth about the binary you are shipping and never a
re-derivation that can drift from it.

## Machine-readable output

Every command takes `--json`, so the CLI is usable from a script, a CI step or
an assistant without parsing a table.

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
