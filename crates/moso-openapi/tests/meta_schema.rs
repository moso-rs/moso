//! Structural conformance against the official OpenAPI 3.1 meta-schema.
//!
//! # What this test proves
//!
//! It assembles a deliberately broad document out of `moso-openapi`'s own
//! builders — several paths, every parameter location, a request body, JSON,
//! empty, problem, validation-problem, binary and redirect responses, HTTP
//! bearer and API-key security schemes, servers, tags, external docs, a
//! shared response and a webhook — serialises it exactly as the crate would
//! serve it, and validates that JSON against the OpenAPI Initiative's own
//! OpenAPI 3.1 meta-schema. A field the emitter spells wrong, a required member
//! it forgets, a status key it puts in the wrong place, or any property the
//! specification's `unevaluatedProperties: false` walls forbid is a hard
//! failure here rather than a caveat on the guide page.
//!
//! # What it does *not* prove, and why
//!
//! The fixture is the OAI **base** schema
//! (`tests/fixtures/openapi-3.1-schema-2022-10-07.json`, provenance in the
//! sibling `PROVENANCE.md`). Its own description is "the description of OpenAPI
//! v3.1.x documents *without schema validation*": every embedded Schema Object
//! is checked only against `{ "type": ["object", "boolean"] }`, not against the
//! full JSON Schema 2020-12 dialect. So this test confirms that the *document
//! envelope* — paths, operations, parameters, responses, components wiring,
//! security, servers, tags and `x-*` extensions — is well formed, and that each
//! `components/schemas` entry is a JSON object; it does **not** re-validate the
//! internals of each generated schema against 2020-12. Doing that requires the
//! `schema-base` variant, which `$ref`s the OpenAPI dialect and its vocabulary
//! meta-schemas and cannot be resolved without I/O or bundling several more
//! files. `moso-schema` owns 2020-12 conformance of the schemas it emits.
//!
//! # Offline by construction
//!
//! The base schema's only outward reference is its `$schema` keyword naming
//! JSON Schema 2020-12, which the `jsonschema` crate ships built in; the
//! `spec.openapis.org/.../dialect/base` URL appears only as the default value
//! of `jsonSchemaDialect`, never as a `$ref`. The crate is pulled with
//! `default-features = false`, so its HTTP/file retrievers are not even
//! compiled: there is no code path that could reach the network.

use moso::prelude::Schema;
use moso_openapi::{
    ContentType, DocumentBuilder, HttpMethod, Param, ResponseSpec, SecurityRequirement,
    SecurityScheme,
};
use serde_json::Value;

/// The OpenAPI 3.1 meta-schema, committed under `tests/fixtures/`.
const META_SCHEMA: &str = include_str!("fixtures/openapi-3.1-schema-2022-10-07.json");

/// A create payload, so the request body is a real registered `$ref`.
#[derive(Schema)]
struct CreateWidget {
    /// Human-readable name.
    name: String,
    /// How many are in stock.
    quantity: u32,
}

/// A widget as the API returns one.
#[derive(Schema)]
struct WidgetOut {
    /// Stable identifier.
    id: u64,
    /// Human-readable name.
    name: String,
}

/// Build a document that exercises as much of the emitter as one document can.
fn representative_document() -> Value {
    let mut builder = DocumentBuilder::new();
    builder
        .title("Widget API")
        .version("1.4.0")
        .summary("A deliberately broad document for meta-schema conformance.")
        .description("Exercises every response family and parameter location.")
        .license_spdx("MIT")
        .contact("Widgets Team", "widgets@example.com")
        .server("https://api.widgets.example", "production")
        .server("http://localhost:8080", "local development")
        .external_docs("https://docs.widgets.example", "Full guide")
        .tag_description("widgets", "Everything about widgets")
        .security_scheme("bearer", SecurityScheme::http_bearer("JWT"))
        .security_scheme("api_key", SecurityScheme::api_key_header("X-Api-Key"))
        .security(SecurityRequirement::scheme("bearer"))
        .extension("x-audience", "internal");

    // A reusable 404 in components.responses, referenced by ResponseSpec::shared.
    let mut not_found = moso_openapi::Response::new("The widget does not exist.");
    not_found.content.insert(
        ContentType::ProblemJson.as_str().to_owned(),
        moso_openapi::MediaType::new(moso_openapi::SchemaNode::reference(
            "#/components/schemas/Problem",
        )),
    );
    builder.shared_response("NotFound", not_found);

    // GET /widgets — list, with query and header parameters.
    builder.operation(HttpMethod::Get, "/widgets", |op| {
        op.summary("List widgets")
            .operation_id("list_widgets")
            .tag("widgets")
            .parameter(
                Param::query("limit")
                    .description("Page size.")
                    .schema_of::<u32>(),
            )
            .parameter(
                Param::header("x-request-id")
                    .description("Correlation id.")
                    .schema_of::<String>(),
            )
            .response(200, ResponseSpec::json_of::<WidgetOut>())
            .public();
    });

    // POST /widgets — create, with a request body and a validation problem.
    builder.operation(HttpMethod::Post, "/widgets", |op| {
        op.summary("Create a widget")
            .operation_id("create_widget")
            .tag("widgets")
            .request_body_of::<CreateWidget>(ContentType::Json, true)
            .response(201, ResponseSpec::json_of::<WidgetOut>())
            .response(422, ResponseSpec::validation_problem_of::<CreateWidget>())
            .security(SecurityRequirement::scheme("bearer"));
    });

    // GET /widgets/{id} — path parameter, problem response, shared 404.
    builder.operation(HttpMethod::Get, "/widgets/{id}", |op| {
        op.summary("Fetch one widget")
            .operation_id("get_widget")
            .tag("widgets")
            .parameter(Param::path("id").schema_of::<u64>())
            .response(200, ResponseSpec::json_of::<WidgetOut>())
            .response(404, ResponseSpec::shared("NotFound"));
    });

    // DELETE /widgets/{id} — empty 204 and a cookie parameter.
    builder.operation(HttpMethod::Delete, "/widgets/{id}", |op| {
        op.summary("Delete a widget")
            .operation_id("delete_widget")
            .tag("widgets")
            .deprecated()
            .parameter(Param::path("id").schema_of::<u64>())
            .parameter(Param::cookie("session").schema_of::<String>())
            .response(204, ResponseSpec::empty("Deleted."))
            .response(404, ResponseSpec::problem("No such widget."));
    });

    // GET /widgets/{id}/export — a binary download and a redirect.
    builder.operation(HttpMethod::Get, "/widgets/{id}/export", |op| {
        op.summary("Export a widget")
            .operation_id("export_widget")
            .tag("widgets")
            .parameter(Param::path("id").schema_of::<u64>())
            .response(200, ResponseSpec::binary("The exported bytes."))
            .response(303, ResponseSpec::redirect("Follow the location."));
    });

    // A webhook, so the `webhooks` map is populated too.
    let mut webhook = moso_openapi::OperationBuilder::new(moso_openapi::SchemaGenerator::new(
        moso_openapi::COMPONENTS_SCHEMAS_PREFIX,
    ));
    webhook
        .summary("A widget changed")
        .operation_id("widget_changed")
        .request_body_of::<WidgetOut>(ContentType::Json, true)
        .response(200, ResponseSpec::empty("Acknowledged."));
    builder.webhook("widget-changed", HttpMethod::Post, webhook.into_spec());

    let document = builder.build().expect("a well-formed document");
    serde_json::to_value(&document).expect("the document serialises")
}

#[test]
fn an_assembled_document_conforms_to_the_openapi_31_meta_schema() {
    let schema: Value =
        serde_json::from_str(META_SCHEMA).expect("the committed meta-schema is valid JSON");
    let validator =
        jsonschema::validator_for(&schema).expect("the OpenAPI 3.1 meta-schema compiles offline");

    let instance = representative_document();

    let failures: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| format!("  at `{}`: {error}", error.instance_path()))
        .collect();

    assert!(
        failures.is_empty(),
        "the assembled document violates the OpenAPI 3.1 meta-schema:\n{}",
        failures.join("\n"),
    );
}

#[test]
fn the_meta_schema_fixture_selects_the_2020_12_dialect() {
    // A guard on the fixture itself: if a future edit swapped in a variant with
    // a different `$schema`, the offline guarantee documented above would no
    // longer hold, and this test says so in one line rather than letting the
    // conformance test fail obscurely.
    let schema: Value = serde_json::from_str(META_SCHEMA).expect("valid JSON");
    assert_eq!(
        schema.get("$schema").and_then(Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema"),
        "the fixture must stay on the JSON Schema 2020-12 dialect the validator bundles",
    );
}
