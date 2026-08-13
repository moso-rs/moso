# Test fixtures - provenance

## `openapi-3.1-schema-2022-10-07.json`

The official OpenAPI 3.1 meta-schema, published by the OpenAPI Initiative.

- Source: <https://spec.openapis.org/oas/3.1/schema/2022-10-07>
- `$id`: `https://spec.openapis.org/oas/3.1/schema/2022-10-07`
- Retrieved: 2026-08 (byte-for-byte as published; not edited).
- Licence: the OpenAPI Specification schemas are published by the OpenAPI
  Initiative under Apache-2.0.

This is the **base** OpenAPI 3.1 schema - the one whose own description reads
"The description of OpenAPI v3.1.x documents *without schema validation*". It
validates the whole OpenAPI document structure (info, paths, operations,
parameters, responses, components, security, servers, tags, extensions) but
deliberately treats every embedded Schema Object loosely: its `$defs.schema`
subschema is `{ "$dynamicAnchor": "meta", "type": ["object", "boolean"] }`, so a
Schema Object only has to *be* an object or a boolean, not conform to the full
JSON Schema 2020-12 dialect.

The only reference the file makes outside itself is the `$schema` keyword naming
`https://json-schema.org/draft/2020-12/schema`, which selects the dialect. The
`jsonschema` crate ships that meta-schema built in, so the file compiles and
validates entirely offline - no network, no `spec.openapis.org/oas/3.1/dialect/base`
retrieval (that URL appears only as the *default value* of `jsonSchemaDialect`,
never as a `$ref`).

The sibling `schema-base` variant, which swaps the loose `$defs.schema` for a
`$ref` to the OpenAPI 3.1 dialect and would validate Schema Objects against the
full 2020-12 vocabulary, is intentionally **not** used here: it pulls the
dialect and vocabulary meta-schemas as external references, which cannot be
resolved without either bundling several more files or performing I/O. What the
base schema covers offline - the entire document envelope - is exactly the
structural conformance the test claims.
