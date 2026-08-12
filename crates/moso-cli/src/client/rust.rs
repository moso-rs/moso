//! The Rust target: three files, `serde` and `serde_json` and nothing else.
//!
//! # Why it is transport-agnostic
//!
//! The generated client *describes* requests — method, URL, unencoded query
//! pairs, headers, body — and hands them to a `Transport` you implement once
//! over whatever HTTP client the rest of your program already uses. It performs
//! no I/O.
//!
//! Generating against `reqwest` instead was the obvious alternative and was
//! rejected for three reasons. A client crate that names an HTTP crate also
//! names a TLS stack, and Moso has an opinion there (rustls, never OpenSSL)
//! that a code generator has no business imposing on somebody else's binary. A
//! program that already has a configured client — with its pool, its timeouts,
//! its retry policy and its tracing — wants the generated code to use *that*
//! one, not a second one. And the pasteable manifest snippet stays two lines
//! with no feature matrix, which is the difference between "paste this" and
//! "read the feature table first".
//!
//! The cost is one `impl Transport` per program, about fifteen lines, written
//! out in the generated module's own documentation.
//!
//! # What it produces
//!
//! | File | Contents |
//! | --- | --- |
//! | `mod.rs` | the module doc, the manifest snippet, the `Transport` example |
//! | `models.rs` | one `struct`, `enum` or alias per component schema |
//! | `client.rs` | the runtime, the argument structs, and `Client<T>` |
//!
//! # Two deliberate simplifications
//!
//! An integer is `i64` and a number is `f64`, because the document's `format`
//! is advisory and a `u64` above `i64::MAX` is not representable in JSON's
//! number type anyway. And an absent member and a `null` one both decode to
//! `None`: distinguishing them needs a second `Option` layer at every optional
//! nullable field, which costs every caller a `Some(None)` to read one bit
//! almost no API means anything by.
//!
//! # Triple backticks
//!
//! A description that reaches a generated doc comment has its triple backticks
//! reduced to double ones. An unbalanced or Rust-tagged fence inside a doc
//! comment is a *doctest* in the user's crate, and a generated client must not
//! be able to fail somebody's test run with prose.

use std::collections::{BTreeMap, BTreeSet};

use super::{Emitted, header_lines, wrap};
use crate::client::model::{
    Additional, Api, Body, Media, NamedType, Object, Operation, Parameter, Place, Property,
    ResponseCase, Returns, Style, Type,
};
use crate::naming::{to_pascal, to_snake};

/// The transport-agnostic plumbing every generated client shares.
const RUNTIME: &str = include_str!("../../templates/client/runtime.rs.tpl");

/// Words that cannot be an identifier, so a field or function named after one
/// is emitted in its raw form.
const KEYWORDS: &[&str] = &[
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "crate",
    "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl", "in",
    "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// Words that are keywords but cannot be written `r#`-prefixed either.
const UNRAWABLE: &[&str] = &["crate", "self", "Self", "super"];

/// Generate the Rust client.
pub fn emit(api: &Api) -> Vec<Emitted> {
    let cycles = Cycles::new(api);
    vec![
        Emitted {
            path: "mod.rs".to_owned(),
            contents: mod_file(api),
        },
        Emitted {
            path: "models.rs".to_owned(),
            contents: models_file(api, &cycles),
        },
        Emitted {
            path: "client.rs".to_owned(),
            contents: client_file(api, &cycles),
        },
    ]
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// `mod.rs`: what to paste into `Cargo.toml`, and how to wire a transport.
fn mod_file(api: &Api) -> String {
    let mut out = String::new();
    for line in header_lines(api, "rust", "the module root", &api.notes) {
        push_line(&mut out, "//!", &line);
    }
    out.push_str("//!\n");
    // Prose is wrapped; the fenced blocks are not, because rewrapping code
    // would break the very snippet a reader is meant to paste.
    let mut fenced = false;
    for line in module_doc(api) {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            push_line(&mut out, "//!", &line);
            continue;
        }
        if fenced {
            push_line(&mut out, "//!", &line);
            continue;
        }
        for wrapped in wrap(&line, 84) {
            push_line(&mut out, "//!", &wrapped);
        }
    }

    out.push_str(
        "\n// Generated code names every type it can build, and a program that calls two of\n\
         // twenty operations should not have to hear about the other eighteen.\n\
         #![allow(dead_code)]\n\n",
    );
    out.push_str("pub mod client;\npub mod models;\n\n");
    // Glob re-exports so that every generated name — the runtime, the argument
    // structs and the models alike — is reachable as `api::Thing`. They cannot
    // collide: one allocator hands out every name in both files.
    out.push_str("pub use client::*;\n");
    if !api.types.is_empty() {
        out.push_str("pub use models::*;\n");
    }
    out
}

/// The prose that teaches a reader how to use what was generated.
fn module_doc(api: &Api) -> Vec<String> {
    let mut lines = vec![
        format!("A typed client for {} {}.", api.title, api.version),
        String::new(),
    ];
    if let Some(description) = &api.description {
        lines.push(description.clone());
        lines.push(String::new());
    }
    lines.extend([
        "# Adding it to a crate".to_owned(),
        String::new(),
        "```toml".to_owned(),
        "[dependencies]".to_owned(),
        "serde = { version = \"1\", features = [\"derive\"] }".to_owned(),
        "serde_json = \"1\"".to_owned(),
        "```".to_owned(),
        String::new(),
        "Nothing else. This client describes requests rather than performing them, so it \
         pulls in no HTTP crate and makes no TLS decision on your behalf."
            .to_owned(),
        String::new(),
        "# Wiring it to an HTTP client".to_owned(),
        String::new(),
        "```ignore".to_owned(),
        "// Not compiled as a doctest: it names `reqwest`, which this module".to_owned(),
        "// deliberately does not depend on.".to_owned(),
        "struct Reqwest(reqwest::Client);".to_owned(),
        String::new(),
        "impl Transport for Reqwest {".to_owned(),
        "    type Error = reqwest::Error;".to_owned(),
        String::new(),
        "    async fn send(&self, request: ApiRequest) -> Result<ApiResponse, Self::Error> {"
            .to_owned(),
        "        let mut builder = self".to_owned(),
        "            .0".to_owned(),
        "            .request(request.method.parse().expect(\"a valid method\"), request.url)"
            .to_owned(),
        "            .query(&request.query);".to_owned(),
        "        for (name, value) in request.headers {".to_owned(),
        "            builder = builder.header(name, value);".to_owned(),
        "        }".to_owned(),
        "        if let Some(body) = request.body {".to_owned(),
        "            builder = builder.header(\"content-type\", body.content_type).body(body.bytes);"
            .to_owned(),
        "        }".to_owned(),
        String::new(),
        "        let response = builder.send().await?;".to_owned(),
        "        let status = response.status().as_u16();".to_owned(),
        "        let headers = response".to_owned(),
        "            .headers()".to_owned(),
        "            .iter()".to_owned(),
        "            .map(|(name, value)| {".to_owned(),
        "                (name.to_string(), value.to_str().unwrap_or_default().to_owned())"
            .to_owned(),
        "            })".to_owned(),
        "            .collect();".to_owned(),
        "        let body = response.bytes().await?.to_vec();".to_owned(),
        String::new(),
        "        Ok(ApiResponse { status, headers, body })".to_owned(),
        "    }".to_owned(),
        "}".to_owned(),
        String::new(),
        "let client = Client::new(Reqwest(reqwest::Client::new()), DEFAULT_BASE_URL);".to_owned(),
        "```".to_owned(),
        String::new(),
        "# Failure".to_owned(),
        String::new(),
        "Every method returns `Result<_, ApiError<T::Error>>`. `ApiError::Problem` carries the \
         RFC 9457 document the server sent, so `problem.kind` identifies the class and \
         `problem.has_code(\"len\")` answers which constraint failed — neither needs a \
         string to be matched on."
            .to_owned(),
    ]);
    lines
}

/// `models.rs`: the component schemas.
fn models_file(api: &Api, cycles: &Cycles) -> String {
    let mut out = String::new();
    for line in header_lines(api, "rust", "the types the API exchanges", &[]) {
        push_line(&mut out, "//!", &line);
    }
    if api.types.is_empty() {
        out.push_str("\n// This document declares no schemas.\n");
        return out;
    }
    for named in &api.types {
        out.push('\n');
        out.push_str(&declaration(named, cycles));
    }
    out
}

/// `client.rs`: the runtime, the argument structs, and `Client<T>`.
fn client_file(api: &Api, cycles: &Cycles) -> String {
    let mut out = String::new();
    for line in header_lines(api, "rust", "the client itself", &[]) {
        push_line(&mut out, "//!", &line);
    }

    out.push('\n');
    if !api.types.is_empty() {
        out.push_str("use super::models::*;\n\n");
    }
    out.push_str("/// Where requests go unless you pass another base to [`Client::new`].\n");
    out.push_str(&format!(
        "pub const DEFAULT_BASE_URL: &str = {};\n\n",
        string_literal(api.base_url.as_deref().unwrap_or(""))
    ));
    out.push_str(RUNTIME);

    out.push_str(
        "\n// -------------------------------------------------------------------------\n\
         // Arguments\n\
         // -------------------------------------------------------------------------\n",
    );
    for operation in &api.operations {
        if let Some(text) = params_struct(operation, cycles) {
            out.push('\n');
            out.push_str(&text);
        }
    }

    out.push_str(
        "\n// -------------------------------------------------------------------------\n\
         // The client\n\
         // -------------------------------------------------------------------------\n\n",
    );
    out.push_str(
        "/// Every operation this API declares, over a transport of your choosing.\n\
         #[derive(Debug, Clone)]\n\
         pub struct Client<T> {\n    \
         transport: T,\n    \
         base_url: String,\n\
         }\n\n\
         impl<T> Client<T> {\n    \
         /// Build a client over `transport`, reaching the API at `base_url`.\n    \
         pub fn new(transport: T, base_url: impl Into<String>) -> Self {\n        \
         let mut base_url = base_url.into();\n        \
         while base_url.ends_with('/') {\n            \
         base_url.pop();\n        \
         }\n        \
         Self { transport, base_url }\n    \
         }\n\n    \
         /// The transport underneath, for the request this client cannot make.\n    \
         pub fn transport(&self) -> &T {\n        \
         &self.transport\n    \
         }\n\
         }\n\n",
    );

    out.push_str("impl<T: Transport> Client<T> {\n");
    for (index, operation) in api.operations.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&method(operation, cycles));
    }
    if api.operations.is_empty() {
        out.push_str("    // This document declares no operations.\n");
    }
    out.push_str("}\n");
    out
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// One component schema, as a `struct`, an `enum` or an alias.
fn declaration(named: &NamedType, cycles: &Cycles) -> String {
    let mut out = String::new();
    let mut lines = Vec::new();
    if let Some(description) = &named.description {
        lines.push(description.clone());
    }
    if let Some(schema) = &named.schema_name
        && schema != &named.name
    {
        lines.push(format!("Schema `{schema}` of the OpenAPI document."));
    }
    // Marked in prose rather than with `#[deprecated]`: the attribute fires
    // wherever the type is *named*, which for a generated model is its own
    // `Serialize` impl, so it would warn about code the user did not write.
    if named.deprecated {
        lines.push("**Deprecated.** The document marks this schema deprecated.".to_owned());
    }
    doc(&mut out, "", &lines);

    match &named.ty {
        Type::Object(object) => structure(&mut out, &named.name, object, cycles),
        Type::Enum(values) => enumeration(&mut out, &named.name, values),
        Type::Union(members) => variants(&mut out, &named.name, members, cycles),
        Type::Every(members) => intersection(&mut out, &named.name, members, cycles),
        other => {
            out.push_str(&format!(
                "pub type {} = {};\n",
                named.name,
                render(other, Some(&named.name), true, cycles)
            ));
        }
    }
    out
}

/// A struct with declared fields.
fn structure(out: &mut String, name: &str, object: &Object, cycles: &Cycles) {
    out.push_str("#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]\n");
    out.push_str(&format!("pub struct {name} {{\n"));
    let mut taken = BTreeSet::new();
    for property in &object.properties {
        field(out, property, name, &mut taken, cycles);
    }
    match &object.additional {
        Additional::Closed => {}
        Additional::Open | Additional::Typed(_) => {
            let value = match &object.additional {
                Additional::Typed(value) => render(value, Some(name), false, cycles),
                _ => "serde_json::Value".to_owned(),
            };
            let ident = unique(&mut taken, "extra");
            out.push_str("    /// Every member the schema does not name.\n");
            out.push_str("    #[serde(flatten)]\n");
            out.push_str(&format!(
                "    pub {ident}: std::collections::BTreeMap<String, {value}>,\n"
            ));
        }
    }
    out.push_str("}\n");
}

/// One field of a struct.
fn field(
    out: &mut String,
    property: &Property,
    owner: &str,
    taken: &mut BTreeSet<String>,
    cycles: &Cycles,
) {
    let mut lines = Vec::new();
    if let Some(description) = &property.description {
        lines.push(description.clone());
    }
    if property.read_only {
        lines.push("Sent by the server; ignored on the way in (`readOnly`).".to_owned());
    }
    if property.write_only {
        lines.push("Accepted on the way in; never sent back (`writeOnly`).".to_owned());
    }
    if property.deprecated {
        lines.push("**Deprecated.**".to_owned());
    }
    doc(out, "    ", &lines);

    let ident = unique(taken, &to_snake(&property.name));
    let mut rendered = render(&property.ty, Some(owner), true, cycles);
    let nullable = matches!(property.ty, Type::Nullable(_));
    if !property.required && !nullable {
        rendered = format!("Option<{rendered}>");
    }

    let mut attributes = Vec::new();
    if bare(&ident) != property.name {
        attributes.push(format!("rename = {}", string_literal(&property.name)));
    }
    if !property.required {
        attributes.push("default".to_owned());
        attributes.push("skip_serializing_if = \"Option::is_none\"".to_owned());
    }
    if !attributes.is_empty() {
        out.push_str(&format!("    #[serde({})]\n", attributes.join(", ")));
    }
    out.push_str(&format!("    pub {ident}: {rendered},\n"));
}

/// A closed set of string values.
fn enumeration(out: &mut String, name: &str, values: &[serde_json::Value]) {
    let strings: Vec<&str> = values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    if strings.len() != values.len() {
        out.push_str(
            "// The document enumerates values that are not all strings, and a Rust enum\n\
             // over mixed scalars would not round-trip; the values are carried as JSON.\n",
        );
        out.push_str(&format!("pub type {name} = serde_json::Value;\n"));
        return;
    }

    out.push_str("#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]\n");
    out.push_str(&format!("pub enum {name} {{\n"));
    let mut taken = BTreeSet::new();
    for value in strings {
        let variant = unique(&mut taken, &variant_name(value));
        if bare(&variant) != value {
            out.push_str(&format!(
                "    #[serde(rename = {})]\n",
                string_literal(value)
            ));
        }
        out.push_str(&format!("    {variant},\n"));
    }
    out.push_str("}\n");
}

/// A `oneOf` or `anyOf`, as an untagged enum.
fn variants(out: &mut String, name: &str, members: &[Type], cycles: &Cycles) {
    out.push_str("/// Matched in order: the first member that decodes wins.\n");
    out.push_str("#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]\n");
    out.push_str("#[serde(untagged)]\n");
    out.push_str(&format!("pub enum {name} {{\n"));
    let mut taken = BTreeSet::new();
    for (index, member) in members.iter().enumerate() {
        let hint = match member {
            Type::Ref(target) => target.clone(),
            other => variant_hint(other, index),
        };
        let variant = unique(&mut taken, &hint);
        out.push_str(&format!(
            "    {variant}({}),\n",
            render(member, Some(name), true, cycles)
        ));
    }
    out.push_str("}\n");
}

/// An `allOf`, as a struct of flattened parts.
fn intersection(out: &mut String, name: &str, members: &[Type], cycles: &Cycles) {
    if members.iter().any(|member| !matches!(member, Type::Ref(_))) {
        out.push_str(
            "// `allOf` here composes something other than named object schemas, which a\n\
             // Rust struct cannot flatten; the value is carried as JSON.\n",
        );
        out.push_str(&format!("pub type {name} = serde_json::Value;\n"));
        return;
    }

    out.push_str("#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]\n");
    out.push_str(&format!("pub struct {name} {{\n"));
    let mut taken = BTreeSet::new();
    for member in members {
        let Type::Ref(target) = member else {
            continue;
        };
        let ident = unique(&mut taken, &to_snake(target));
        out.push_str(&format!("    /// The `{target}` half of this schema.\n"));
        out.push_str("    #[serde(flatten)]\n");
        out.push_str(&format!(
            "    pub {ident}: {},\n",
            render(member, Some(name), true, cycles)
        ));
    }
    out.push_str("}\n");
}

// ---------------------------------------------------------------------------
// Type expressions
// ---------------------------------------------------------------------------

/// Render a type as a Rust expression.
///
/// `direct` says whether the value would be stored inline; a `Vec` or a
/// `BTreeMap` already puts its contents behind a pointer, so a reference from
/// inside one cannot make a type infinitely sized.
fn render(ty: &Type, owner: Option<&str>, direct: bool, cycles: &Cycles) -> String {
    match ty {
        Type::Unknown | Type::Object(_) | Type::Union(_) | Type::Every(_) | Type::Enum(_) => {
            "serde_json::Value".to_owned()
        }
        Type::Null => "()".to_owned(),
        Type::Boolean => "bool".to_owned(),
        Type::Integer => "i64".to_owned(),
        Type::Number => "f64".to_owned(),
        Type::Text => "String".to_owned(),
        Type::Binary => "String".to_owned(),
        Type::List(item) => format!("Vec<{}>", render(item, owner, false, cycles)),
        Type::Map(value) => format!(
            "std::collections::BTreeMap<String, {}>",
            render(value, owner, false, cycles)
        ),
        Type::Nullable(inner) => format!("Option<{}>", render(inner, owner, direct, cycles)),
        Type::Ref(name) => {
            if direct && owner.is_some_and(|owner| cycles.recurses(name, owner)) {
                format!("Box<{name}>")
            } else {
                name.clone()
            }
        }
        // The reason travels in a block comment rather than a doc comment
        // because this is a type *expression*: it can be four levels inside a
        // `Option<Vec<..>>`, and the reader needs it where the `Value` is.
        Type::Opaque(reason) => format!("serde_json::Value /* {} */", comment_safe(reason)),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// The `{Operation}Params` struct, when the operation takes parameters.
fn params_struct(operation: &Operation, cycles: &Cycles) -> Option<String> {
    let name = operation.params_name.as_ref()?;
    if operation.parameters.is_empty() {
        return None;
    }

    let mut out = String::new();
    doc(
        &mut out,
        "",
        &[format!("Arguments for [`Client::{}`].", operation.name)],
    );
    let every_optional = operation
        .parameters
        .iter()
        .all(|parameter| !parameter.required);
    out.push_str(if every_optional {
        "#[derive(Debug, Clone, Default)]\n"
    } else {
        "#[derive(Debug, Clone)]\n"
    });
    out.push_str(&format!("pub struct {name} {{\n"));

    let mut taken = BTreeSet::new();
    for parameter in &operation.parameters {
        let mut lines = Vec::new();
        if let Some(description) = &parameter.description {
            lines.push(description.clone());
        }
        lines.push(format!(
            "Travels in the {}, as `{}`.",
            parameter.place.as_str(),
            parameter.name
        ));
        if parameter.deprecated {
            lines.push("**Deprecated.**".to_owned());
        }
        doc(&mut out, "    ", &lines);
        let ident = unique(&mut taken, &to_snake(&parameter.name));
        let mut rendered = render(&parameter.ty, None, true, cycles);
        if !parameter.required && !matches!(parameter.ty, Type::Nullable(_)) {
            rendered = format!("Option<{rendered}>");
        }
        out.push_str(&format!("    pub {ident}: {rendered},\n"));
    }
    out.push_str("}\n");
    Some(out)
}

/// One method on `Client<T>`.
fn method(operation: &Operation, cycles: &Cycles) -> String {
    let mut out = String::new();
    doc(&mut out, "    ", &operation_doc(operation));
    if operation.deprecated {
        out.push_str("    #[deprecated]\n");
    }

    let takes_params = operation.params_name.is_some() && !operation.parameters.is_empty();
    let mut arguments = vec!["&self".to_owned()];
    if takes_params {
        let name = operation.params_name.clone().unwrap_or_default();
        arguments.push(format!("params: &{name}"));
    }
    if let Some(body) = &operation.body {
        arguments.push(format!("body: {}", body_argument(body, cycles)));
    }

    out.push_str(&format!(
        "    pub async fn {}({}) -> Result<{}, ApiError<T::Error>> {{\n",
        function_name(&operation.name),
        arguments.join(", "),
        success_type(operation, cycles)
    ));

    let query: Vec<&Parameter> = operation
        .parameters
        .iter()
        .filter(|parameter| parameter.place == Place::Query)
        .collect();
    let headers: Vec<&Parameter> = operation
        .parameters
        .iter()
        .filter(|parameter| parameter.place == Place::Header)
        .collect();

    out.push_str(&declare("query", !query.is_empty()));
    let mut taken = BTreeSet::new();
    let mut idents = BTreeMap::new();
    for parameter in &operation.parameters {
        idents.insert(
            (parameter.place, parameter.name.clone()),
            unique(&mut taken, &to_snake(&parameter.name)),
        );
    }
    for parameter in &query {
        let ident = idents
            .get(&(parameter.place, parameter.name.clone()))
            .cloned()
            .unwrap_or_default();
        out.push_str(&format!(
            "        push_query(&mut query, {}, &params.{ident}, QueryStyle::{});\n",
            string_literal(&parameter.name),
            style_name(parameter.style)
        ));
    }
    out.push_str(&declare("headers", !headers.is_empty()));
    for parameter in &headers {
        let ident = idents
            .get(&(parameter.place, parameter.name.clone()))
            .cloned()
            .unwrap_or_default();
        out.push_str(&format!(
            "        push_header(&mut headers, {}, &params.{ident});\n",
            string_literal(&parameter.name)
        ));
    }

    if let Some(body) = &operation.body {
        out.push_str(&body_expression(body));
    }

    out.push_str("        let request = ApiRequest {\n");
    out.push_str(&format!("            method: \"{}\",\n", operation.method));
    out.push_str(&format!(
        "            url: {},\n",
        url_expression(operation, &idents)
    ));
    out.push_str("            query,\n            headers,\n");
    out.push_str(if operation.body.is_some() {
        "            body,\n"
    } else {
        "            body: None,\n"
    });
    out.push_str("        };\n");
    out.push_str(
        "        let response = self.transport.send(request).await.map_err(ApiError::Transport)?;\n",
    );
    out.push_str(&format!(
        "        {}(response)\n    }}\n",
        decoder(operation)
    ));
    out
}

/// `let [mut] name: Vec<(String, String)> = Vec::new();`
fn declare(name: &str, mutable: bool) -> String {
    format!(
        "        let {}{name}: Vec<(String, String)> = Vec::new();\n",
        if mutable { "mut " } else { "" }
    )
}

/// The `format!` that builds the URL.
fn url_expression(operation: &Operation, idents: &BTreeMap<(Place, String), String>) -> String {
    let mut template = String::from("{}");
    let mut arguments = vec!["self.base_url".to_owned()];
    let mut rest = operation.path.as_str();

    while let Some(open) = rest.find('{') {
        template.push_str(&escape_format(&rest[..open]));
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                match idents.get(&(Place::Path, name.to_owned())) {
                    Some(ident) => {
                        template.push_str("{}");
                        arguments.push(format!("encode_path(&params.{ident})"));
                    }
                    None => template.push_str(&escape_format(&format!("{{{name}}}"))),
                }
                rest = &after[close + 1..];
            }
            None => {
                template.push_str(&escape_format(after));
                rest = "";
            }
        }
    }
    template.push_str(&escape_format(rest));

    format!(
        "format!({}, {})",
        string_literal(&template),
        arguments.join(", ")
    )
}

/// Double any brace that must survive `format!` as itself.
fn escape_format(text: &str) -> String {
    text.replace('{', "{{").replace('}', "}}")
}

/// The type of the `body` argument.
fn body_argument(body: &Body, cycles: &Cycles) -> String {
    let inner = match body.media {
        Media::Json => format!("&{}", render(&body.ty, None, true, cycles)),
        Media::Text => "&str".to_owned(),
        Media::Binary => "Vec<u8>".to_owned(),
        _ => "ApiBody".to_owned(),
    };
    if body.required {
        inner
    } else {
        format!("Option<{inner}>")
    }
}

/// The statement that turns the `body` argument into an `ApiBody`.
fn body_expression(body: &Body) -> String {
    let build = |value: &str| match body.media {
        Media::Json => format!("ApiBody::json({value}).map_err(ApiError::Encode)?"),
        Media::Text => format!("ApiBody::text({value})"),
        Media::Binary => format!("ApiBody::binary({value})"),
        _ => value.to_owned(),
    };
    if body.required {
        format!("        let body = Some({});\n", build("body"))
    } else {
        format!(
            "        let body = match body {{\n            \
             Some(value) => Some({}),\n            \
             None => None,\n        \
             }};\n",
            build("value")
        )
    }
}

/// The type a successful call yields.
fn success_type(operation: &Operation, cycles: &Cycles) -> String {
    match &operation.returns {
        Returns::Nothing => "()".to_owned(),
        Returns::Json { ty, optional } => {
            let rendered = render(ty, None, false, cycles);
            if *optional {
                format!("Option<{rendered}>")
            } else {
                rendered
            }
        }
        Returns::Text => "String".to_owned(),
        Returns::Binary => "Vec<u8>".to_owned(),
        Returns::Raw(_) => "ApiResponse".to_owned(),
    }
}

/// Which runtime decoder reads the answer.
fn decoder(operation: &Operation) -> &'static str {
    match &operation.returns {
        Returns::Nothing => "decode_nothing",
        Returns::Json {
            optional: false, ..
        } => "decode_json",
        Returns::Json { optional: true, .. } => "decode_optional_json",
        Returns::Text => "decode_text",
        Returns::Binary => "decode_bytes",
        Returns::Raw(_) => "decode_raw",
    }
}

/// The doc comment above one method.
fn operation_doc(operation: &Operation) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(summary) = &operation.summary {
        lines.push(summary.clone());
    }
    if let Some(description) = &operation.description {
        lines.push(String::new());
        lines.push(description.clone());
    }
    lines.push(String::new());
    lines.push(format!("`{} {}`", operation.method, operation.path));

    if !operation.security.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Requires: {}.",
            operation
                .security
                .iter()
                .map(|scheme| format!("`{scheme}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Returns::Raw(reason) = &operation.returns {
        lines.push(String::new());
        lines.push(format!("The whole [`ApiResponse`] is returned: {reason}."));
    }
    for note in &operation.notes {
        lines.push(String::new());
        lines.push(format!("Note: {note}."));
    }

    lines.push(String::new());
    lines.push("# Errors".to_owned());
    lines.push(
        "`ApiError::Transport` when the request could not be sent, `ApiError::Malformed` \
         when the answer is not what the document promised, and `ApiError::Problem` \
         for a documented failure."
            .to_owned(),
    );
    let documented = describe(&operation.failures);
    if !documented.is_empty() {
        lines.push(String::new());
        lines.push("Documented failures:".to_owned());
        for case in documented {
            lines.push(format!("- {case}"));
        }
    }
    lines
}

/// Response statuses, with their descriptions, as one line each.
fn describe(cases: &[ResponseCase]) -> Vec<String> {
    cases
        .iter()
        .map(|case| match &case.description {
            Some(description) => format!(
                "{} — {}",
                case.status.label(),
                description.replace('\n', " ")
            ),
            None => case.status.label(),
        })
        .collect()
}

/// The `QueryStyle` variant for one style.
const fn style_name(style: Style) -> &'static str {
    match style {
        Style::Form => "Form",
        Style::FormJoined => "FormJoined",
        Style::Deep => "DeepObject",
        Style::Space => "SpaceDelimited",
        Style::Pipe => "PipeDelimited",
    }
}

// ---------------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------------

/// Which named types can reach which others without passing through a `Vec` or
/// a `BTreeMap`.
///
/// A schema that contains itself — a comment with replies, a tree node with
/// children — is an infinitely sized Rust type unless the recursive edge goes
/// through a `Box`. Boxing every reference would be safe and would make every
/// construction noisy, so only the edges that actually close a loop are boxed.
struct Cycles {
    /// Type name to every type reachable from it by value.
    reach: BTreeMap<String, BTreeSet<String>>,
}

impl Cycles {
    /// Compute the closure over one API.
    fn new(api: &Api) -> Self {
        let mut reach: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for named in &api.types {
            let mut direct = BTreeSet::new();
            contained(&named.ty, &mut direct);
            reach.insert(named.name.clone(), direct);
        }

        // Fixpoint. The graph is one node per schema, so a naive closure is
        // cheaper than the machinery that would replace it.
        loop {
            let mut grew = false;
            let snapshot = reach.clone();
            for targets in reach.values_mut() {
                let mut added = BTreeSet::new();
                for target in targets.iter() {
                    if let Some(further) = snapshot.get(target) {
                        for name in further {
                            if !targets.contains(name) {
                                added.insert(name.clone());
                            }
                        }
                    }
                }
                if !added.is_empty() {
                    grew = true;
                    targets.extend(added);
                }
            }
            if !grew {
                break;
            }
        }

        Self { reach }
    }

    /// Whether storing a `target` inside an `owner` would close a loop.
    fn recurses(&self, target: &str, owner: &str) -> bool {
        target == owner
            || self
                .reach
                .get(target)
                .is_some_and(|reachable| reachable.contains(owner))
    }
}

/// Every named type one type stores inline.
fn contained(ty: &Type, out: &mut BTreeSet<String>) {
    match ty {
        Type::Ref(name) => {
            out.insert(name.clone());
        }
        Type::Nullable(inner) => contained(inner, out),
        Type::Union(members) | Type::Every(members) => {
            for member in members {
                contained(member, out);
            }
        }
        Type::Object(object) => {
            for property in &object.properties {
                contained(&property.ty, out);
            }
        }
        // `Vec` and `BTreeMap` already hold their contents behind a pointer.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// A comment or doc-comment line, or the bare marker when it is empty.
fn push_line(out: &mut String, marker: &str, line: &str) {
    if line.is_empty() {
        out.push_str(marker);
        out.push('\n');
    } else {
        out.push_str(&format!("{marker} {line}\n"));
    }
}

/// A `///` block, or nothing when there is nothing to say.
fn doc(out: &mut String, indent: &str, lines: &[String]) {
    let expanded: Vec<String> = lines
        .iter()
        .flat_map(|line| wrap(&fence_safe(line), 88 - indent.len()))
        .collect();
    for line in &expanded {
        push_line(out, &format!("{indent}///"), line);
    }
}

/// Neutralise anything that would close a block comment early, and flatten it
/// onto one line, since the comment is spliced into a type expression.
fn comment_safe(text: &str) -> String {
    text.replace("*/", "*\\/")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reduce a triple backtick, which would open a doctest, to a double one.
fn fence_safe(text: &str) -> String {
    let mut out = text.to_owned();
    while out.contains("```") {
        out = out.replace("```", "``");
    }
    out
}

/// A Rust string literal.
fn string_literal(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// An identifier, raw-prefixed when it is a keyword.
fn identifier(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_owned();
    let cleaned = if cleaned.is_empty() || cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        format!("field_{cleaned}")
    } else {
        cleaned
    };
    if KEYWORDS.contains(&cleaned.as_str()) {
        if UNRAWABLE.contains(&cleaned.as_str()) {
            format!("{cleaned}_")
        } else {
            format!("r#{cleaned}")
        }
    } else {
        cleaned
    }
}

/// An identifier without its raw prefix, which is what serde sees.
fn bare(ident: &str) -> &str {
    ident.strip_prefix("r#").unwrap_or(ident)
}

/// Claim an identifier, suffixing until it is free.
fn unique(taken: &mut BTreeSet<String>, wanted: &str) -> String {
    let base = identifier(wanted);
    if taken.insert(base.clone()) {
        return base;
    }
    for suffix in 2..1000u32 {
        let candidate = format!("{base}_{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    base
}

/// A function name, raw-prefixed when it is a keyword.
fn function_name(name: &str) -> String {
    identifier(name)
}

/// The variant name for one enumerated string.
fn variant_name(value: &str) -> String {
    let pascal = to_pascal(value);
    if pascal.is_empty() || pascal.starts_with(|c: char| c.is_ascii_digit()) {
        format!("Value{pascal}")
    } else {
        pascal
    }
}

/// The variant name for a union member that is not a reference.
fn variant_hint(ty: &Type, index: usize) -> String {
    match ty {
        Type::Boolean => "Boolean".to_owned(),
        Type::Integer => "Integer".to_owned(),
        Type::Number => "Number".to_owned(),
        Type::Text | Type::Binary => "Text".to_owned(),
        Type::List(_) => "List".to_owned(),
        Type::Map(_) => "Map".to_owned(),
        _ => format!("Variant{}", index + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::model::Api;
    use serde_json::json;

    fn generate(document: &serde_json::Value) -> Vec<Emitted> {
        emit(&Api::parse(document).expect("the document parses"))
    }

    fn file<'a>(files: &'a [Emitted], path: &str) -> &'a str {
        files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.contents.as_str())
            .unwrap_or_else(|| panic!("no {path} was generated"))
    }

    fn document(extra: serde_json::Value) -> serde_json::Value {
        let mut base = json!({
            "openapi": "3.1.1",
            "info": {"title": "Shop", "version": "1.0.0"},
            "paths": {},
            "components": {"schemas": {}},
        });
        if let Some(map) = extra.as_object() {
            for (key, value) in map {
                base[key] = value.clone();
            }
        }
        base
    }

    // ── models ────────────────────────────────────────────────────────────

    #[test]
    fn an_object_becomes_a_struct_with_serde_on_it() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "CreatePost": {
                    "type": "object",
                    "properties": {"title": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}},
                    "required": ["title"],
                },
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("pub struct CreatePost {"), "{models}");
        assert!(models.contains("    pub title: String,"), "{models}");
        assert!(
            models.contains("#[serde(default, skip_serializing_if = \"Option::is_none\")]"),
            "{models}"
        );
        assert!(
            models.contains("    pub tags: Option<Vec<String>>,"),
            "{models}"
        );
    }

    #[test]
    fn a_field_named_after_a_keyword_is_raw_and_needs_no_rename() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Problem": {"type": "object", "properties": {"type": {"type": "string"}}, "required": ["type"]},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("    pub r#type: String,"), "{models}");
        assert!(!models.contains("rename = \"type\""), "{models}");
    }

    #[test]
    fn a_field_whose_wire_name_is_not_snake_case_carries_a_rename() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Head": {"type": "object", "properties": {"x-tenant": {"type": "string"}}, "required": ["x-tenant"]},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(
            models.contains("#[serde(rename = \"x-tenant\")]"),
            "{models}"
        );
        assert!(models.contains("    pub x_tenant: String,"), "{models}");
    }

    #[test]
    fn a_string_enum_becomes_an_enum_and_a_mixed_one_stays_json() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "State": {"type": "string", "enum": ["draft", "published"]},
                "Mixed": {"enum": ["a", 1]},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("pub enum State {"), "{models}");
        assert!(models.contains("#[serde(rename = \"draft\")]"), "{models}");
        assert!(models.contains("    Draft,"), "{models}");
        assert!(
            models.contains("pub type Mixed = serde_json::Value;"),
            "{models}"
        );
    }

    #[test]
    fn one_of_becomes_an_untagged_enum() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "A": {"type": "object", "properties": {"a": {"type": "string"}}},
                "Either": {"oneOf": [{"$ref": "#/components/schemas/A"}, {"type": "integer"}]},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("#[serde(untagged)]"), "{models}");
        assert!(models.contains("pub enum Either {"), "{models}");
        assert!(models.contains("    A(A),"), "{models}");
        assert!(models.contains("    Integer(i64),"), "{models}");
    }

    #[test]
    fn all_of_becomes_a_struct_of_flattened_parts() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Base": {"type": "object", "properties": {"id": {"type": "string"}}},
                "Extended": {"allOf": [
                    {"$ref": "#/components/schemas/Base"},
                    {"type": "object", "properties": {"extra": {"type": "string"}}},
                ]},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("pub struct Extended {"), "{models}");
        assert!(models.contains("#[serde(flatten)]"), "{models}");
        assert!(models.contains("    pub base: Base,"), "{models}");
    }

    #[test]
    fn a_recursive_schema_is_boxed_only_where_the_loop_closes() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "parent": {"$ref": "#/components/schemas/Node"},
                        "children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}},
                    },
                },
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(
            models.contains("pub parent: Option<Box<Node>>,"),
            "{models}"
        );
        assert!(
            models.contains("pub children: Option<Vec<Node>>,"),
            "{models}"
        );
    }

    #[test]
    fn additional_properties_become_a_flattened_map() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Problem": {
                    "type": "object",
                    "properties": {"title": {"type": "string"}},
                    "additionalProperties": true,
                },
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(
            models.contains("pub extra: std::collections::BTreeMap<String, serde_json::Value>,"),
            "{models}"
        );
    }

    // ── operations ────────────────────────────────────────────────────────

    fn shop() -> serde_json::Value {
        document(json!({
            "servers": [{"url": "https://api.example.com/"}],
            "paths": {
                "/posts": {
                    "get": {
                        "operationId": "posts_list",
                        "summary": "List posts.",
                        "parameters": [
                            {"name": "limit", "in": "query", "schema": {"type": "integer"}},
                            {"name": "x-tenant", "in": "header", "schema": {"type": "string"}},
                        ],
                        "responses": {"200": {"description": "ok", "content": {"application/json": {
                            "schema": {"type": "array", "items": {"$ref": "#/components/schemas/PostOut"}}}}}},
                    },
                    "post": {
                        "operationId": "posts_create",
                        "requestBody": {"required": true, "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/CreatePost"}}}},
                        "responses": {"201": {"description": "made", "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/PostOut"}}}}},
                    },
                },
                "/posts/{id}": {
                    "delete": {
                        "operationId": "posts_destroy",
                        "parameters": [{"name": "id", "in": "path", "required": true,
                                        "schema": {"type": "string"}}],
                        "responses": {"204": {"description": "gone"}},
                    },
                },
            },
            "components": {"schemas": {
                "PostOut": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
                "CreatePost": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]},
            }},
        }))
    }

    #[test]
    fn each_operation_becomes_one_async_method() {
        let client = file(&generate(&shop()), "client.rs").to_owned();
        assert!(
            client.contains(
                "pub async fn posts_list(&self, params: &PostsListParams) \
                 -> Result<Vec<PostOut>, ApiError<T::Error>> {"
            ),
            "{client}"
        );
        assert!(
            client.contains(
                "pub async fn posts_create(&self, body: &CreatePost) \
                 -> Result<PostOut, ApiError<T::Error>> {"
            ),
            "{client}"
        );
        assert!(
            client.contains(
                "pub async fn posts_destroy(&self, params: &PostsDestroyParams) \
                 -> Result<(), ApiError<T::Error>> {"
            ),
            "{client}"
        );
    }

    #[test]
    fn a_path_parameter_is_encoded_into_the_url() {
        let client = file(&generate(&shop()), "client.rs").to_owned();
        assert!(
            client
                .contains("url: format!(\"{}/posts/{}\", self.base_url, encode_path(&params.id)),"),
            "{client}"
        );
        assert!(
            client.contains("url: format!(\"{}/posts\", self.base_url),"),
            "{client}"
        );
    }

    #[test]
    fn query_and_header_parameters_go_through_the_runtime_helpers() {
        let client = file(&generate(&shop()), "client.rs").to_owned();
        assert!(
            client.contains("push_query(&mut query, \"limit\", &params.limit, QueryStyle::Form);"),
            "{client}"
        );
        assert!(
            client.contains("push_header(&mut headers, \"x-tenant\", &params.x_tenant);"),
            "{client}"
        );
        // A list that is never pushed to is not declared `mut`.
        assert!(
            client.contains("let query: Vec<(String, String)> = Vec::new();"),
            "{client}"
        );
    }

    #[test]
    fn the_module_root_carries_the_manifest_snippet_and_the_transport_example() {
        let root = file(&generate(&shop()), "mod.rs").to_owned();
        assert!(
            root.contains("//! serde = { version = \"1\", features = [\"derive\"] }"),
            "{root}"
        );
        assert!(root.contains("//! serde_json = \"1\""), "{root}");
        assert!(root.contains("//! impl Transport for Reqwest {"), "{root}");
        assert!(root.contains("//! ```ignore"), "{root}");
        assert!(root.contains("pub mod models;"), "{root}");
        // Every generated name is reachable as `api::Thing`, argument structs
        // included — those live in `client.rs` beside the method that takes them.
        assert!(root.contains("pub use client::*;"), "{root}");
        assert!(root.contains("pub use models::*;"), "{root}");
    }

    #[test]
    fn an_unrepresentable_construct_says_why_where_the_reader_meets_it() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "pair": {"type": "array", "prefixItems": [{"type": "string"}]},
                        "external": {"$ref": "https://example.com/Other.json"},
                    },
                },
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(models.contains("prefixItems"), "{models}");
        assert!(
            models.contains("pub external: Option<serde_json::Value /* "),
            "{models}"
        );
    }

    #[test]
    fn a_triple_backtick_in_a_description_cannot_become_a_doctest() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Odd": {"type": "string", "description": "like this:\n```\nnot rust\n```"},
            }},
        })));
        let models = file(&files, "models.rs");
        assert!(!models.contains("/// ```"), "{models}");
        assert!(models.contains("/// ``"), "{models}");
    }

    #[test]
    fn a_document_with_nothing_in_it_still_produces_three_files() {
        let files = generate(&document(json!({})));
        assert_eq!(files.len(), 3);
        assert!(file(&files, "models.rs").contains("declares no schemas"));
        assert!(file(&files, "client.rs").contains("declares no operations"));
        assert!(!file(&files, "mod.rs").contains("pub use models::*;"));
    }

    #[test]
    fn generating_twice_produces_the_same_bytes() {
        let first = generate(&shop());
        let second = generate(&shop());
        assert_eq!(first, second);
    }

    #[test]
    fn identifiers_are_made_legal_without_losing_the_wire_name() {
        assert_eq!(identifier("type"), "r#type");
        assert_eq!(identifier("self"), "self_");
        assert_eq!(identifier("2fa"), "field_2fa");
        assert_eq!(identifier("x-tenant"), "x_tenant");
        assert_eq!(bare("r#type"), "type");

        let mut taken = BTreeSet::new();
        assert_eq!(unique(&mut taken, "id"), "id");
        assert_eq!(unique(&mut taken, "id"), "id_2");
    }

    #[test]
    fn a_brace_in_a_path_that_is_not_a_parameter_survives_the_format() {
        let operation = Operation {
            name: "odd".to_owned(),
            params_name: None,
            method: "GET".to_owned(),
            path: "/a/{missing}/b".to_owned(),
            summary: None,
            description: None,
            deprecated: false,
            security: Vec::new(),
            parameters: Vec::new(),
            body: None,
            returns: Returns::Nothing,
            problem: None,
            success: Vec::new(),
            failures: Vec::new(),
            notes: Vec::new(),
        };
        let rendered = url_expression(&operation, &BTreeMap::new());
        assert_eq!(rendered, "format!(\"{}/a/{{missing}}/b\", self.base_url)");
    }
}
