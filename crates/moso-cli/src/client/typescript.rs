//! The TypeScript target: three files, no dependencies, `fetch` and nothing
//! else.
//!
//! # What it produces
//!
//! | File | Contents |
//! | --- | --- |
//! | `types.ts` | one `interface` or `type` per component schema |
//! | `client.ts` | the runtime, one argument type per operation, and `createClient` |
//! | `index.ts` | `export *` over both, so a consumer imports one path |
//!
//! # Why a result rather than an exception
//!
//! Every method resolves to an [`ApiResult`-shaped] discriminated union rather
//! than rejecting. A rejected promise carries a value TypeScript types as
//! `unknown`, so branching on it means casting, and casting is exactly what a
//! generated client exists to remove. `if (!result.ok)` narrows to the failure
//! arm, `failure.kind` narrows to *which* failure, and `failure.problem` is
//! typed as the union of the schemas that operation actually documents for its
//! error statuses — so `problem.type` and `problem.errors[n].code`, the two
//! things `16-errors.md` promises a client can branch on, are both reachable
//! without a cast.
//!
//! [`ApiResult`-shaped]: https://moso.rs/guides/openapi
//!
//! # Erasable syntax only
//!
//! The output uses no `enum`, no `namespace` and no parameter properties, so it
//! passes through a type-stripping loader (`node --experimental-strip-types`,
//! esbuild, swc) unchanged. That is what lets the test suite check the
//! generated file for syntactic validity without a TypeScript installation.

use std::collections::BTreeSet;

use super::{Emitted, header_lines, wrap};
use crate::client::model::{
    Additional, Api, Body, Media, NamedType, Object, Operation, Parameter, Place, Property,
    ResponseCase, Returns, Style, Type,
};

/// The `fetch`-shaped plumbing every generated client shares.
///
/// A file rather than something assembled per document: it does not vary, so it
/// is diffed once when this generator changes rather than on every
/// regeneration, and it can be linted as the TypeScript it is.
const RUNTIME: &str = include_str!("../../templates/client/runtime.ts");

/// The argument group a parameter lands in, which is also the key the runtime
/// reads it under.
const fn group(place: Place) -> &'static str {
    match place {
        Place::Path => "path",
        Place::Query => "query",
        Place::Header => "headers",
    }
}

/// Generate the TypeScript client.
pub fn emit(api: &Api) -> Vec<Emitted> {
    vec![
        Emitted {
            path: "types.ts".to_owned(),
            contents: types_file(api),
        },
        Emitted {
            path: "client.ts".to_owned(),
            contents: client_file(api),
        },
        Emitted {
            path: "index.ts".to_owned(),
            contents: index_file(api),
        },
    ]
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// `types.ts`: the component schemas.
fn types_file(api: &Api) -> String {
    let mut out = String::new();
    banner(&mut out, api, "the types the API exchanges", &[]);

    if api.types.is_empty() {
        out.push_str("// This document declares no schemas.\nexport {};\n");
        return out;
    }

    for named in &api.types {
        out.push('\n');
        declaration(&mut out, named);
    }
    out
}

/// `client.ts`: the runtime, the argument types and `createClient`.
fn client_file(api: &Api) -> String {
    let mut out = String::new();
    banner(&mut out, api, "the client itself", &api.notes);

    let imported = imports(api);
    if !imported.is_empty() {
        out.push_str("\nimport type {\n");
        for name in &imported {
            out.push_str(&format!("  {name},\n"));
        }
        out.push_str("} from \"./types.js\";\n");
    }

    out.push_str("\n/** Where requests go when `ClientOptions.baseUrl` is not given. */\n");
    out.push_str(&format!(
        "const DEFAULT_BASE_URL = {};\n",
        string_literal(api.base_url.as_deref().unwrap_or(""))
    ));

    out.push('\n');
    out.push_str(RUNTIME);

    if !api.operations.is_empty() {
        out.push_str(
            "\n// -------------------------------------------------------------------------\n",
        );
        out.push_str("// Arguments\n");
        out.push_str(
            "// -------------------------------------------------------------------------\n",
        );
        for operation in &api.operations {
            if let Some(text) = params_interface(operation) {
                out.push('\n');
                out.push_str(&text);
            }
        }
    }

    out.push_str(
        "\n// -------------------------------------------------------------------------\n",
    );
    out.push_str("// The client\n");
    out.push_str(
        "// -------------------------------------------------------------------------\n\n",
    );
    out.push_str("/** Every operation this API declares. */\nexport interface Client {\n");
    for (index, operation) in api.operations.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        signature(&mut out, operation);
    }
    if api.operations.is_empty() {
        out.push_str("  /** This document declares no operations. */\n");
        out.push_str("  readonly _empty?: never;\n");
    }
    out.push_str("}\n\n");

    out.push_str("/** Build a client. Every method resolves; none of them reject. */\n");
    out.push_str("export function createClient(options: ClientOptions = {}): Client {\n");
    out.push_str("  return {\n");
    for operation in &api.operations {
        implementation(&mut out, operation);
    }
    out.push_str("  };\n}\n");
    out
}

/// `index.ts`: one import path for a consumer.
fn index_file(api: &Api) -> String {
    let mut out = String::new();
    banner(&mut out, api, "one import path for everything above", &[]);
    out.push_str("\nexport * from \"./types.js\";\nexport * from \"./client.js\";\n");
    out
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

/// One component schema.
fn declaration(out: &mut String, named: &NamedType) {
    let mut lines = Vec::new();
    if let Some(description) = &named.description {
        lines.push(description.clone());
    }
    if let Some(schema) = &named.schema_name
        && schema != &named.name
    {
        lines.push(format!("Schema `{schema}` of the OpenAPI document."));
    }
    if named.deprecated {
        lines.push("@deprecated".to_owned());
    }
    doc(out, "", &lines);

    match &named.ty {
        Type::Object(object) if object.additional == Additional::Closed => {
            out.push_str(&format!("export interface {} ", named.name));
            out.push_str(&object_body(object, 0));
            out.push('\n');
        }
        other => {
            out.push_str(&format!(
                "export type {} = {};\n",
                named.name,
                render(other, 0)
            ));
        }
    }
}

/// The `{ .. }` of an object, without the intersection an open object needs.
fn object_body(object: &Object, depth: usize) -> String {
    let outer = "  ".repeat(depth);
    let inner = "  ".repeat(depth + 1);
    if object.properties.is_empty() {
        return "{}".to_owned();
    }

    let mut out = String::from("{\n");
    for property in &object.properties {
        member(&mut out, property, &inner, depth + 1);
    }
    out.push_str(&outer);
    out.push('}');
    out
}

/// One property of an object literal type.
fn member(out: &mut String, property: &Property, indent: &str, depth: usize) {
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
        lines.push("@deprecated".to_owned());
    }
    doc(out, indent, &lines);

    let optional = if property.required { "" } else { "?" };
    out.push_str(&format!(
        "{indent}{}{optional}: {};\n",
        property_key(&property.name),
        render(&property.ty, depth)
    ));
}

// ---------------------------------------------------------------------------
// Type expressions
// ---------------------------------------------------------------------------

/// Render a type as a TypeScript expression.
fn render(ty: &Type, depth: usize) -> String {
    match ty {
        Type::Unknown => "unknown".to_owned(),
        Type::Null => "null".to_owned(),
        Type::Boolean => "boolean".to_owned(),
        Type::Integer | Type::Number => "number".to_owned(),
        Type::Text => "string".to_owned(),
        Type::Binary => "Blob".to_owned(),
        Type::Enum(values) => values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(" | "),
        Type::List(item) => {
            let rendered = render(item, depth);
            if rendered.contains(' ') || rendered.contains('|') {
                format!("Array<{rendered}>")
            } else {
                format!("{rendered}[]")
            }
        }
        Type::Map(value) => format!("{{ [key: string]: {} }}", render(value, depth)),
        Type::Object(object) => {
            let body = object_body(object, depth);
            match &object.additional {
                Additional::Closed => body,
                Additional::Open => format!("{body} & {{ [key: string]: unknown }}"),
                Additional::Typed(value) => {
                    format!("{body} & {{ [key: string]: {} }}", render(value, depth))
                }
            }
        }
        Type::Ref(name) => name.clone(),
        Type::Nullable(inner) => {
            let rendered = render(inner, depth);
            if rendered.contains('|') && !rendered.starts_with('(') {
                format!("({rendered}) | null")
            } else {
                format!("{rendered} | null")
            }
        }
        Type::Union(members) => members
            .iter()
            .map(|member| render(member, depth))
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Every(members) => members
            .iter()
            .map(|member| render(member, depth))
            .collect::<Vec<_>>()
            .join(" & "),
        Type::Opaque(reason) => format!("unknown /* {} */", comment_safe(reason)),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// The `{Operation}Params` interface, when the operation takes anything.
fn params_interface(operation: &Operation) -> Option<String> {
    let name = operation.params_name.as_ref()?;
    let mut out = String::new();
    doc(
        &mut out,
        "",
        &[format!("Arguments for `{}`.", camel(&operation.name))],
    );
    out.push_str(&format!("export interface {name} {{\n"));

    for place in [Place::Path, Place::Query, Place::Header] {
        let travelling: Vec<&Parameter> = operation
            .parameters
            .iter()
            .filter(|parameter| parameter.place == place)
            .collect();
        if travelling.is_empty() {
            continue;
        }
        let optional = travelling.iter().all(|parameter| !parameter.required);
        out.push_str(&format!(
            "  readonly {}{}: {{\n",
            group(place),
            if optional { "?" } else { "" }
        ));
        for parameter in travelling {
            let mut lines = Vec::new();
            if let Some(description) = &parameter.description {
                lines.push(description.clone());
            }
            if parameter.deprecated {
                lines.push("@deprecated".to_owned());
            }
            doc(&mut out, "    ", &lines);
            out.push_str(&format!(
                "    readonly {}{}: {};\n",
                property_key(&parameter.name),
                if parameter.required { "" } else { "?" },
                render(&parameter.ty, 2)
            ));
        }
        out.push_str("  };\n");
    }

    if let Some(body) = &operation.body {
        let mut lines = Vec::new();
        if let Some(description) = &body.description {
            lines.push(description.clone());
        }
        lines.push(format!("Sent as `{}`.", body.media.content_type()));
        doc(&mut out, "  ", &lines);
        out.push_str(&format!(
            "  readonly body{}: {};\n",
            if body.required { "" } else { "?" },
            body_type(body)
        ));
    }

    out.push_str("}\n");
    Some(out)
}

/// The method's line in the `Client` interface.
fn signature(out: &mut String, operation: &Operation) {
    doc(out, "  ", &operation_doc(operation));

    let mut arguments = Vec::new();
    if let Some(name) = &operation.params_name {
        arguments.push(format!(
            "params{}: {name}",
            if params_optional(operation) { "?" } else { "" }
        ));
    }
    arguments.push("init?: RequestInit".to_owned());

    let name = camel(&operation.name);
    let result = format!(
        "Promise<ApiResult<{}, {}>>",
        success_type(operation),
        problem_type(operation)
    );

    // One line while it fits, broken the way a formatter would otherwise break
    // it. The output is committed and reviewed, and a 140-column signature is
    // reviewed by scrolling.
    let single = format!(
        "  readonly {name}: ({}) => {result};\n",
        arguments.join(", ")
    );
    if single.trim_end().chars().count() <= 96 {
        out.push_str(&single);
        return;
    }
    out.push_str(&format!("  readonly {name}: (\n"));
    for argument in &arguments {
        out.push_str(&format!("    {argument},\n"));
    }
    out.push_str(&format!("  ) => {result};\n"));
}

/// The method's entry in the object `createClient` returns.
fn implementation(out: &mut String, operation: &Operation) {
    let takes_params = operation.params_name.is_some();
    let access = if takes_params && params_optional(operation) {
        "params?."
    } else if takes_params {
        "params."
    } else {
        ""
    };

    out.push_str(&format!(
        "    {}: ({}init) =>\n",
        camel(&operation.name),
        if takes_params { "params, " } else { "" }
    ));
    out.push_str(&format!(
        "      call<{}, {}>(options, {{\n",
        success_type(operation),
        problem_type(operation)
    ));
    out.push_str(&format!("        method: \"{}\",\n", operation.method));
    out.push_str(&format!(
        "        template: {},\n",
        string_literal(&operation.path)
    ));

    for place in [Place::Path, Place::Query, Place::Header] {
        if operation
            .parameters
            .iter()
            .any(|parameter| parameter.place == place)
        {
            let key = group(place);
            out.push_str(&format!("        {key}: {access}{key},\n"));
        }
    }

    let styles: Vec<&Parameter> = operation
        .parameters
        .iter()
        .filter(|parameter| parameter.place == Place::Query && parameter.style != Style::Form)
        .collect();
    if !styles.is_empty() {
        let entries: Vec<String> = styles
            .iter()
            .map(|parameter| {
                format!(
                    "{}: \"{}\"",
                    property_key(&parameter.name),
                    parameter.style.as_str()
                )
            })
            .collect();
        out.push_str(&format!("        styles: {{ {} }},\n", entries.join(", ")));
    }

    if let Some(body) = &operation.body {
        out.push_str(&format!("        body: {access}body,\n"));
        out.push_str(&format!("        bodyKind: \"{}\",\n", body_kind(body)));
    }

    out.push_str(&format!("        accept: \"{}\",\n", accept(operation)));
    out.push_str("        init,\n      }),\n");
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

    let documented = |cases: &[ResponseCase]| -> Vec<String> {
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
    };

    for (title, cases) in [
        ("Answers", documented(&operation.success)),
        ("Documented failures", documented(&operation.failures)),
    ] {
        if cases.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push(format!("{title}:"));
        for case in cases {
            lines.push(format!("- {case}"));
        }
    }

    if let Returns::Raw(reason) = &operation.returns {
        lines.push(String::new());
        lines.push(format!("The raw `Response` is returned: {reason}."));
    }
    for note in &operation.notes {
        lines.push(String::new());
        lines.push(format!("Note: {note}."));
    }
    if operation.deprecated {
        lines.push(String::new());
        lines.push("@deprecated".to_owned());
    }
    lines
}

/// Whether every group of arguments is optional.
fn params_optional(operation: &Operation) -> bool {
    operation
        .parameters
        .iter()
        .all(|parameter| !parameter.required)
        && operation.body.as_ref().is_none_or(|body| !body.required)
}

/// The type of a successful response body.
fn success_type(operation: &Operation) -> String {
    match &operation.returns {
        Returns::Nothing => "undefined".to_owned(),
        Returns::Json { ty, optional } => {
            let rendered = render(ty, 0);
            if *optional {
                format!("{rendered} | undefined")
            } else {
                rendered
            }
        }
        Returns::Text => "string".to_owned(),
        Returns::Binary => "Blob".to_owned(),
        Returns::Raw(_) => "Response".to_owned(),
    }
}

/// The type of a documented failure body.
fn problem_type(operation: &Operation) -> String {
    match &operation.problem {
        Some(ty) => render(ty, 0),
        None => "ProblemBody".to_owned(),
    }
}

/// Which decoder the runtime should use.
fn accept(operation: &Operation) -> &'static str {
    match &operation.returns {
        Returns::Nothing => "none",
        Returns::Json { .. } => "json",
        Returns::Text => "text",
        Returns::Binary => "binary",
        Returns::Raw(_) => "response",
    }
}

/// How a request body is encoded.
fn body_kind(body: &Body) -> &'static str {
    match body.media {
        Media::Json => "json",
        Media::Form => "form",
        Media::Text => "text",
        Media::Binary => "binary",
        Media::Multipart | Media::EventStream | Media::Other(_) => "passthrough",
    }
}

/// The TypeScript type of a request body.
fn body_type(body: &Body) -> String {
    match body.media {
        Media::Json | Media::Form => render(&body.ty, 1),
        Media::Text => "string".to_owned(),
        Media::Binary => "Blob".to_owned(),
        Media::Multipart => "FormData".to_owned(),
        Media::EventStream | Media::Other(_) => "BodyInit".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// The header every generated file opens with.
fn banner(out: &mut String, api: &Api, what: &str, notes: &[String]) {
    for line in header_lines(api, "ts", what, notes) {
        if line.is_empty() {
            out.push_str("//\n");
        } else {
            out.push_str(&format!("// {line}\n"));
        }
    }
}

/// A JSDoc block, or nothing when there is nothing to say.
fn doc(out: &mut String, indent: &str, lines: &[String]) {
    let expanded: Vec<String> = lines
        .iter()
        .flat_map(|line| wrap(line, 88 - indent.len()))
        .collect();
    match expanded.len() {
        0 => {}
        1 if !expanded[0].is_empty() => {
            out.push_str(&format!("{indent}/** {} */\n", comment_safe(&expanded[0])));
        }
        _ => {
            out.push_str(&format!("{indent}/**\n"));
            for line in &expanded {
                if line.is_empty() {
                    out.push_str(&format!("{indent} *\n"));
                } else {
                    out.push_str(&format!("{indent} * {}\n", comment_safe(line)));
                }
            }
            out.push_str(&format!("{indent} */\n"));
        }
    }
}

/// Neutralise anything that would close a comment early.
fn comment_safe(text: &str) -> String {
    text.replace("*/", "*\\/")
}

/// A property name, quoted when it is not a bare identifier.
fn property_key(name: &str) -> String {
    let bare = !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        });
    if bare {
        name.to_owned()
    } else {
        string_literal(name)
    }
}

/// A double-quoted string literal, escaped for both TypeScript and JSON.
fn string_literal(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// `posts_list` becomes `postsList`.
fn camel(name: &str) -> String {
    let pascal = crate::naming::to_pascal(name);
    let mut characters = pascal.chars();
    match characters.next() {
        Some(first) => first.to_lowercase().collect::<String>() + characters.as_str(),
        None => pascal,
    }
}

/// The named types `client.ts` mentions, sorted.
fn imports(api: &Api) -> Vec<String> {
    let mut found = BTreeSet::new();
    for operation in &api.operations {
        for parameter in &operation.parameters {
            collect(&parameter.ty, &mut found);
        }
        if let Some(body) = &operation.body {
            collect(&body.ty, &mut found);
        }
        if let Returns::Json { ty, .. } = &operation.returns {
            collect(ty, &mut found);
        }
        if let Some(problem) = &operation.problem {
            collect(problem, &mut found);
        }
    }
    found.into_iter().collect()
}

/// Every `$ref` reachable inside one rendered type expression.
fn collect(ty: &Type, found: &mut BTreeSet<String>) {
    match ty {
        Type::Ref(name) => {
            found.insert(name.clone());
        }
        Type::List(inner) | Type::Map(inner) | Type::Nullable(inner) => collect(inner, found),
        Type::Union(members) | Type::Every(members) => {
            for member in members {
                collect(member, found);
            }
        }
        Type::Object(object) => {
            for property in &object.properties {
                collect(&property.ty, found);
            }
            if let Additional::Typed(value) = &object.additional {
                collect(value, found);
            }
        }
        _ => {}
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

    // ── schemas ───────────────────────────────────────────────────────────

    #[test]
    fn a_closed_object_becomes_an_interface_and_an_open_one_an_intersection() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Closed": {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]},
                "Open": {
                    "type": "object",
                    "properties": {"a": {"type": "string"}},
                    "additionalProperties": true,
                },
            }},
        })));
        let types = file(&files, "types.ts");
        assert!(types.contains("export interface Closed {"), "{types}");
        assert!(types.contains("  a: string;"), "{types}");
        assert!(
            types.contains("export type Open = {")
                && types.contains("& { [key: string]: unknown }"),
            "{types}"
        );
    }

    #[test]
    fn an_optional_property_gets_a_question_mark_and_a_nullable_one_a_null() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Post": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "subtitle": {"type": ["string", "null"]},
                    },
                    "required": ["title"],
                },
            }},
        })));
        let types = file(&files, "types.ts");
        assert!(types.contains("  title: string;"), "{types}");
        assert!(types.contains("  subtitle?: string | null;"), "{types}");
    }

    #[test]
    fn a_property_whose_name_is_not_an_identifier_is_quoted() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Head": {"type": "object", "properties": {"x-tenant": {"type": "string"}}},
            }},
        })));
        assert!(file(&files, "types.ts").contains("  \"x-tenant\"?: string;"));
    }

    #[test]
    fn an_enum_becomes_a_literal_union() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "State": {"type": "string", "enum": ["draft", "published"]},
            }},
        })));
        assert!(
            file(&files, "types.ts").contains("export type State = \"draft\" | \"published\";"),
            "{}",
            file(&files, "types.ts")
        );
    }

    #[test]
    fn an_unrepresentable_construct_is_unknown_and_carries_its_reason() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Pair": {"type": "array", "prefixItems": [{"type": "string"}]},
            }},
        })));
        let types = file(&files, "types.ts");
        assert!(types.contains("unknown /*"), "{types}");
        assert!(types.contains("prefixItems"), "{types}");
    }

    #[test]
    fn a_comment_terminator_inside_a_description_cannot_close_the_comment() {
        let files = generate(&document(json!({
            "components": {"schemas": {
                "Odd": {"type": "string", "description": "ends with */ and keeps going"},
            }},
        })));
        let types = file(&files, "types.ts");
        assert!(!types.contains("*/ and keeps going"), "{types}");
        assert!(types.contains("*\\/ and keeps going"), "{types}");
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
                            {"name": "tag", "in": "query", "style": "form", "explode": false,
                             "schema": {"type": "array", "items": {"type": "string"}}},
                        ],
                        "responses": {
                            "200": {"description": "ok", "content": {"application/json": {
                                "schema": {"type": "array", "items": {"$ref": "#/components/schemas/PostOut"}}}}},
                            "422": {"description": "bad", "content": {"application/problem+json": {
                                "schema": {"$ref": "#/components/schemas/Problem"}}}},
                        },
                    },
                    "post": {
                        "operationId": "posts_create",
                        "security": [{"api_key": []}],
                        "requestBody": {"required": true, "content": {"application/json": {
                            "schema": {"$ref": "#/components/schemas/CreatePost"}}}},
                        "responses": {
                            "201": {"description": "made", "content": {"application/json": {
                                "schema": {"$ref": "#/components/schemas/PostOut"}}}},
                        },
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
                "Problem": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]},
            }},
        }))
    }

    #[test]
    fn each_operation_becomes_one_typed_method() {
        let files = generate(&shop());
        let client = file(&files, "client.ts");
        // Compared with whitespace collapsed: a signature is broken over lines
        // once it is too wide, and this test is about the types, not the width.
        let flat = client.split_whitespace().collect::<Vec<_>>().join(" ");
        for expected in [
            "readonly postsList: ( params?: PostsListParams, init?: RequestInit, ) \
             => Promise<ApiResult<PostOut[], Problem>>;",
            "readonly postsCreate: ( params: PostsCreateParams, init?: RequestInit, ) \
             => Promise<ApiResult<PostOut, ProblemBody>>;",
            "readonly postsDestroy: ( params: PostsDestroyParams, init?: RequestInit, ) \
             => Promise<ApiResult<undefined, ProblemBody>>;",
        ] {
            assert!(flat.contains(expected), "missing `{expected}` in\n{client}");
        }
    }

    #[test]
    fn a_signature_that_fits_is_left_on_one_line() {
        let files = generate(&document(json!({
            "paths": {"/a": {"get": {"operationId": "ping", "responses": {
                "204": {"description": "pong"},
            }}}},
        })));
        assert!(
            file(&files, "client.ts").contains(
                "  readonly ping: (init?: RequestInit) \
                 => Promise<ApiResult<undefined, ProblemBody>>;\n"
            ),
            "{}",
            file(&files, "client.ts")
        );
    }

    #[test]
    fn arguments_are_grouped_by_where_they_travel() {
        let files = generate(&shop());
        let client = file(&files, "client.ts");
        assert!(
            client.contains("export interface PostsListParams {"),
            "{client}"
        );
        assert!(client.contains("  readonly query?: {"), "{client}");
        assert!(client.contains("    readonly limit?: number;"), "{client}");
        assert!(
            client.contains("export interface PostsDestroyParams {"),
            "{client}"
        );
        assert!(client.contains("  readonly path: {"), "{client}");
        assert!(client.contains("    readonly id: string;"), "{client}");
        assert!(client.contains("  readonly body: CreatePost;"), "{client}");
    }

    #[test]
    fn a_non_default_query_style_is_carried_into_the_call() {
        let client = file(&generate(&shop()), "client.ts").to_owned();
        assert!(
            client.contains("styles: { tag: \"formJoined\" }"),
            "{client}"
        );
        assert!(
            !client.contains("limit: \"form\""),
            "the default is not restated"
        );
    }

    #[test]
    fn the_base_url_comes_from_the_document_without_its_trailing_slash() {
        let client = file(&generate(&shop()), "client.ts").to_owned();
        assert!(
            client.contains("const DEFAULT_BASE_URL = \"https://api.example.com\";"),
            "{client}"
        );
    }

    #[test]
    fn only_the_types_the_client_mentions_are_imported() {
        let client = file(&generate(&shop()), "client.ts").to_owned();
        let header = client
            .split("const DEFAULT_BASE_URL")
            .next()
            .unwrap_or_default();
        assert!(header.contains("  CreatePost,"), "{header}");
        assert!(header.contains("  PostOut,"), "{header}");
        assert!(header.contains("  Problem,"), "{header}");
    }

    #[test]
    fn the_security_and_the_documented_statuses_reach_the_doc_comment() {
        let client = file(&generate(&shop()), "client.ts").to_owned();
        assert!(client.contains("Requires: `api_key`."), "{client}");
        assert!(client.contains("Documented failures:"), "{client}");
        assert!(client.contains("- 422 — bad"), "{client}");
    }

    #[test]
    fn a_document_with_nothing_in_it_still_produces_files_that_parse() {
        let files = generate(&document(json!({})));
        assert!(file(&files, "types.ts").contains("export {};"));
        assert!(file(&files, "client.ts").contains("export function createClient"));
        assert!(file(&files, "index.ts").contains("export * from \"./types.js\";"));
    }

    #[test]
    fn generating_twice_produces_the_same_bytes() {
        let first = generate(&shop());
        let second = generate(&shop());
        for (left, right) in first.iter().zip(second.iter()) {
            assert_eq!(left.path, right.path);
            assert_eq!(left.contents, right.contents);
        }
    }
}
