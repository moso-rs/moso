# moso-openapi

**The OpenAPI 3.1 document model, the builders that assemble it, and the
self-hosted documentation UIs that render it.**

Nobody writes an OpenAPI fragment by hand. `OperationBuilder` is the hinge:
every extractor, every response type, every guard and every request-scoped
dependency writes into one, and the operation that comes out is the sum of what
the handler's *types* said about it.

```rust
use moso_openapi::{OperationBuilder, Param, ResponseSpec};
use moso_schema::json_schema::SchemaGenerator;

let mut op = OperationBuilder::new(SchemaGenerator::default());

op.summary("List users").tag("users");
op.parameter(Param::query("limit").schema_of::<u32>().required(false));
op.response(200, ResponseSpec::json_of::<Vec<String>>());

let (spec, _generator) = op.finish();
assert_eq!(spec.parameters.len(), 1);
```

## Merge semantics

Several describers contribute to one operation and they must not fight:

| Member | Rule |
| --- | --- |
| `summary`, `description`, `operationId`, `externalDocs` | **first writer wins** |
| `tags` | appended, deduplicated, insertion order preserved |
| `parameters` | keyed by `(in, name)`; first wins, later fills only absent members |
| `requestBody` | first wins; later calls add *content types* it did not describe |
| `responses` | keyed by status; first wins, later fills only absent members |
| `security` | appended unless an identical requirement is already present |
| `deprecated`, `hidden`, `validated` | sticky: once true, always true |

`#[endpoint]` writes its summary first, so an extractor can never overwrite the
words a developer put in the doc comment.

## Determinism

Every map is an `IndexMap` and schemas are emitted in a stable order, so a
committed `openapi.json` diffs cleanly and `moso openapi check` can tell a real
contract change from map-iteration noise.

## Documentation UIs

The assets are **vendored, never fetched from a CDN** - air-gapped deployments
are a supported segment.

| Feature | Default | UI |
| --- | --- | --- |
| `scalar` | yes | Scalar |
| `redoc` | no | ReDoc |
| `swagger-ui` | no | Swagger UI |

## Relationship to the rest of Moso

Depends on [`moso-schema`](../moso-schema) for the JSON Schema model and embeds
generated schemas into `components/schemas`. It knows nothing about HTTP
handlers; [`moso-core`](../moso-core) is what drives it while walking a composed
router.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
