//! Boot-assembly benchmark: how long does it take to assemble the OpenAPI
//! document for a ~200-operation application?
//!
//! # Why this exists
//!
//! `App::build()` assembles the whole OpenAPI document once, at boot, out of
//! the operations every mounted route contributed. The design budget is **under
//! 15 ms for 200 endpoints** (see the OpenAPI guide's "what is not there"). This
//! benchmark puts a number behind that budget: it synthesises 200 operations —
//! each with a path parameter, a query parameter, a registered request body and
//! three responses referencing generated component schemas — through
//! `moso-openapi`'s own [`DocumentBuilder`], then measures a single
//! `build()` that folds the schema generator into `components.schemas`, applies
//! the canonical ordering and runs every consistency check.
//!
//! It measures the cost `moso-core` pays at boot for this crate's work; it does
//! not include route registration in Axum or provider-map validation, which
//! live in `moso-core`. The number is reported to stdout by criterion; it is
//! deliberately **not** asserted against 15 ms, because a wall-clock threshold
//! is a flaky gate across CI hardware. The measurement existing is what closes
//! the "boot cost is unmeasured" caveat; regressions are read from the printed
//! trend, not enforced here.
//!
//! Run it with `cargo bench -p moso-openapi`.

use criterion::{Criterion, criterion_group, criterion_main};
use moso_openapi::{
    ContentType, DocumentBuilder, HttpMethod, Param, ResponseSpec, SecurityRequirement,
    SecurityScheme,
};
use std::hint::black_box;

/// The total number of operations to assemble, matching the documented budget
/// of 200 endpoints. The loop below registers two operations per iteration.
const OPERATION_COUNT: usize = 200;

/// A create payload, registered once per operation as a distinct schema so the
/// generator does real work rather than deduplicating a single type.
#[derive(moso::prelude::Schema)]
struct Payload {
    /// A name.
    name: String,
    /// A count.
    count: u32,
}

/// An output DTO, likewise registered per operation.
#[derive(moso::prelude::Schema)]
struct Record {
    /// An identifier.
    id: u64,
    /// A label.
    label: String,
}

/// Register `OPERATION_COUNT` operations and assemble the document.
///
/// This is the unit under test: it is deliberately self-contained so the
/// benchmark measures assembly and schema folding, not fixture construction.
fn assemble() {
    let mut builder = DocumentBuilder::new();
    builder
        .title("Synthetic API")
        .version("1.0.0")
        .server("https://api.example", "production")
        .security_scheme("bearer", SecurityScheme::http_bearer("JWT"))
        .security(SecurityRequirement::scheme("bearer"));

    for index in 0..OPERATION_COUNT / 2 {
        let list_path = format!("/resource{index}");
        builder.operation(HttpMethod::Get, list_path, |op| {
            op.summary("List the resource")
                .tag("resource")
                .parameter(Param::query("limit").schema_of::<u32>())
                .response(200, ResponseSpec::json_of::<Record>());
        });

        let item_path = format!("/resource{index}/{{id}}");
        builder.operation(HttpMethod::Post, item_path, |op| {
            op.summary("Create under the resource")
                .tag("resource")
                .parameter(Param::path("id").schema_of::<u64>())
                .request_body_of::<Payload>(ContentType::Json, true)
                .response(201, ResponseSpec::json_of::<Record>())
                .response(422, ResponseSpec::validation_problem_of::<Payload>())
                .response(404, ResponseSpec::problem("Not found."));
        });
    }

    let document = builder.build().expect("a well-formed synthetic document");
    black_box(document);
}

/// Benchmark the assembly of a ~200-operation document.
fn boot_assembly(criterion: &mut Criterion) {
    criterion.bench_function("assemble_200_operations", |bencher| {
        bencher.iter(assemble);
    });
}

criterion_group!(benches, boot_assembly);
criterion_main!(benches);
