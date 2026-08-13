//! Probe: every `::moso::__private::…` path macro output names must resolve.
//!
//! A missing re-export here breaks *every* macro at once, in user code, with a
//! span pointing at generated tokens. This file is the list of paths the four
//! macro modules actually emit — extracted from their `quote!` bodies — so that
//! failure lands here instead, where it costs a second to read.
//!
//! When a macro starts emitting a new path, add it here in the same commit.

#![allow(unused_imports, dead_code)]

use moso::__private::{
    ArrayBuilder, BootError, BootErrors, Bounds, BoxFuture, Coerce, Config, ConfigDescriptor,
    ConfigKey, ConfigLoader, ConstraintError, Dependency, Describe, Discriminator, Endpoint, Error,
    ErrorCode, ErrorKind, Extract, ExtractBody, FieldDescriptor, FieldSpec, HandlerFn, HttpMethod,
    IntoResponse, Next, NoContent, ObjectBuilder, OperationBuilder, Profile, ProviderReq, Request,
    RequestCtx, Response, ResponseSpec, Result, Router, Schema, SchemaGenerator, SchemaNode,
    SchemaRef, StringBuilder, Validate, ValidationCtx, ValidationErrors, check_contains,
    check_ends_with, check_format, check_len_seq, check_len_str, check_multiple_of_f64,
    check_multiple_of_i64, check_nested, check_non_empty_seq, check_non_empty_str, check_one_of,
    check_one_of_i64, check_one_of_str, check_pattern, check_range_f64, check_range_i64,
    check_range_u64, check_starts_with, check_unique, comma_delimited, concat_reqs, describe_json,
    empty_response, generic_schema_name, http, inline_schema_ref, is_valid_format, json_response,
    middleware_ctx, pipe_delimited, regex, route_path, serde, serde_json, set_header,
    space_delimited, tower, validate_path,
};

/// Associated items generated code names, not just the bare types.
#[test]
fn associated_items_resolve() {
    let _ = Bounds::INCLUSIVE;
    let _ = Bounds::EXCLUSIVE_MIN;
    let _ = Bounds::EXCLUSIVE_MAX;
    let _ = ProviderReq::of::<u8>();
    let _ = regex::Regex::new("^a$").expect("valid");
    let _ = SchemaNode::null();
    let _ = ObjectBuilder::new();
    let _ = StringBuilder::new();
    let _ = ArrayBuilder::new();
    let _ = ResponseSpec::empty("No Content");
    let _ = http::StatusCode::OK;
    let _ = serde_json::Value::Null;
}

/// `route_path!` is a `macro_rules!`, so a plain `use` does not prove it works.
#[test]
fn route_path_expands_through_the_facade() {
    let path = moso::__private::route_path!("/users/{id}");
    assert_eq!(path, "/users/{id}");
}

/// `concat_reqs!` likewise.
#[test]
fn concat_reqs_expands_through_the_facade() {
    const A: &[ProviderReq] = &[ProviderReq::of::<u8>()];
    const BOTH: &[ProviderReq] = moso::__private::concat_reqs!(A, A);
    assert_eq!(BOTH.len(), 2);
}
