# @@CRATE_NAME@@

A [Moso](https://github.com/lowsbarrel/moso) application.

## Run it

```sh
cargo run
```

Then:

```sh
curl localhost:3000/
curl -X POST localhost:3000/greetings -H 'content-type: application/json' -d '{"name":"ada"}'
```

## Test it

```sh
cargo test
```

`tests/api.rs` boots the real application through `App::into_service()` and
drives it as a tower service — same router, same middleware, same dependency
graph as production, no port bound.

## The layout

| path                | what lives there                                        |
| ------------------- | ------------------------------------------------------- |
| `src/main.rs`       | the entry point, and nothing else                       |
| `src/lib.rs`        | `AppConfig`, and `build()` — the composition root        |
| `src/routes.rs`     | payload types and handlers                              |
| `src/dump.rs`       | how the `moso` CLI interrogates this binary             |
| `tests/api.rs`      | the HTTP contract                                       |
| `.env.example`      | every environment variable, generated from `AppConfig`  |
| `.cargo/config.toml`| build settings; `moso doctor` explains them             |

## The CLI

The `moso` CLI never links your crate. It runs `cargo run -- --dump-<kind>` and
reads one document off stdout — `src/dump.rs` is the whole protocol.

```sh
moso routes                              # the route table
moso openapi export --out openapi.json   # write the spec
moso openapi check                       # fail if the committed spec is stale
moso config                              # every key, and where its value came from
moso config --env-example                # regenerate .env.example
moso doctor                              # check this machine's toolchain
```

## Configuration

Every field of `AppConfig` is read from `@@ENV_PREFIX@@__<FIELD>`. Copy
`.env.example` to `.env` and edit that; `.env` is git-ignored and is only loaded
outside the `production` profile.

Add a field, then run `moso config --env-example --out .env.example` so the
committed example cannot drift from the struct.
