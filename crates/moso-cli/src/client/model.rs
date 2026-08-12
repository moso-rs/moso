//! The OpenAPI document, reduced to what a client generator needs.
//!
//! # Why there is an intermediate model at all
//!
//! Two emitters read this, TypeScript and Rust, and they disagree about almost
//! everything *except* the questions the document answers: what types exist,
//! which are nullable, which operation takes what and returns what. Reducing
//! the document once means the two emitters cannot drift apart on the hard
//! parts — `oneOf` normalisation, nullability, hoisting an anonymous object
//! into a name — and it means those parts are tested once, here, rather than
//! twice by inspecting generated text.
//!
//! ```text
//! openapi.json ──parse──▶ Api { types, operations } ──emit──▶ *.ts
//!                                                    └─emit──▶ *.rs
//! ```
//!
//! # Determinism
//!
//! `serde_json` is built with `preserve_order` across this workspace, so a JSON
//! object iterates in *document* order rather than sorted order. Every map this
//! module walks is therefore collected and sorted explicitly before it is read:
//! component schemas, object properties, path items, response statuses. Arrays
//! keep the order the document gave them, which is already deterministic and is
//! meaningful for `oneOf` and `enum`. Nothing here reads a clock, an
//! environment variable or a path.
//!
//! # Naming
//!
//! Every type gets one name, shared by both emitters, allocated once through
//! [`NameAllocator`] so that a Rust `PostOut` and a TypeScript `PostOut` are
//! the same word. Anonymous objects, enums, unions and intersections are
//! *hoisted* into named types as they are met, because Rust cannot spell any of
//! them inline; TypeScript could, but a shared name is worth more than an
//! inline literal.
//!
//! # What is refused
//!
//! A construct with no faithful representation becomes [`Type::Opaque`],
//! carrying the sentence explaining why, which both emitters print next to the
//! `unknown` they fall back to. Anything only *partly* represented adds a line
//! to [`Api::notes`], which the command prints and both emitters repeat in the
//! generated file's header. Nothing is dropped in silence.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::exit::{CliError, Outcome};
use crate::naming::{to_pascal, to_snake};

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Identifiers the generated runtimes define, in either language.
///
/// Reserved before any schema is named, so a component called `Client` or
/// `Response` is renamed rather than silently shadowing the plumbing that has
/// to work. The list is the union of both targets' vocabularies plus the DOM
/// globals the TypeScript runtime names, because one shared allocation is what
/// keeps `PostOut` spelled the same way in both outputs.
/// Two rules decide what belongs here. A name the generated runtime *declares*
/// would be redeclared by a schema of the same name. A name the runtime *uses*
/// — a DOM global, a Rust prelude type — would be shadowed by the glob import
/// or the `import type` that brings the schemas in. Both break at compile time
/// rather than silently, but both break, so the schema is renamed instead.
///
/// `Problem` and `ValidationProblem` are deliberately absent: Moso publishes
/// those as component schemas, the runtimes call their own `ProblemBody`, and a
/// generated `Problem` should keep the name the document gave it.
const RESERVED: &[&str] = &[
    "ApiBody",
    "ApiError",
    "ApiFailure",
    "ApiRequest",
    "ApiResponse",
    "ApiResult",
    "Array",
    "Blob",
    "BodyInit",
    "Box",
    "Client",
    "ClientOptions",
    "FetchLike",
    "FormData",
    "Headers",
    "HeadersInit",
    "JSON",
    "Object",
    "Option",
    "ProblemBody",
    "ProblemFieldError",
    "Promise",
    "QueryStyle",
    "Readonly",
    "Record",
    "RequestCredentials",
    "RequestInit",
    "Response",
    "Result",
    "Self",
    "String",
    "Transport",
    "URLSearchParams",
    "Vec",
];

/// Hands out type names that are unique across the whole generated client.
#[derive(Debug, Clone)]
pub struct NameAllocator {
    taken: BTreeSet<String>,
}

impl NameAllocator {
    /// A fresh allocator with the runtime's own identifiers already taken.
    #[must_use]
    pub fn new() -> Self {
        Self {
            taken: RESERVED.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// Claim a name derived from `wanted`, appending a digit on collision.
    ///
    /// Returns the claimed name and, when it is not the obvious one, the note
    /// explaining the rename — collisions are rare and surprising, so they are
    /// reported rather than left for the reader to notice in a diff.
    pub fn allocate(&mut self, wanted: &str) -> (String, Option<String>) {
        let base = match to_pascal(wanted) {
            empty if empty.is_empty() => "Schema".to_owned(),
            pascal => pascal,
        };
        if self.taken.insert(base.clone()) {
            return (base, None);
        }
        for suffix in 2..1000u32 {
            let candidate = format!("{base}{suffix}");
            if self.taken.insert(candidate.clone()) {
                let note = format!("`{wanted}` is generated as `{candidate}`: `{base}` was taken");
                return (candidate, Some(note));
            }
        }
        // Unreachable for any real document; falling back keeps this total.
        (base, None)
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// One API, as a generator sees it.
#[derive(Debug, Clone)]
pub struct Api {
    /// `info.title`.
    pub title: String,
    /// `info.version`.
    pub version: String,
    /// `info.description`.
    pub description: Option<String>,
    /// The first entry of `servers`, with its variables' defaults applied.
    pub base_url: Option<String>,
    /// Every named type, sorted by name.
    pub types: Vec<NamedType>,
    /// Every operation, sorted by name.
    pub operations: Vec<Operation>,
    /// What the document said that this model could not carry across.
    pub notes: Vec<String>,
}

/// A type with a name of its own.
#[derive(Debug, Clone)]
pub struct NamedType {
    /// The identifier both emitters use.
    pub name: String,
    /// The component name it came from, when it came from `components/schemas`.
    pub schema_name: Option<String>,
    /// The schema's `description`.
    pub description: Option<String>,
    /// Whether the schema is marked deprecated.
    pub deprecated: bool,
    /// The shape.
    pub ty: Type,
}

/// A type expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// No type information: `{}`, `true`, or a schema with nothing to go on.
    Unknown,
    /// The JSON `null` type, on its own.
    Null,
    /// `type: boolean`.
    Boolean,
    /// `type: integer`.
    Integer,
    /// `type: number`.
    Number,
    /// `type: string`.
    Text,
    /// `type: string, format: binary` — bytes, not characters.
    Binary,
    /// A closed set of scalar values: `enum`, or `const` as a set of one.
    Enum(Vec<Value>),
    /// `type: array`.
    List(Box<Type>),
    /// An object with no declared properties: a map of this value type.
    Map(Box<Type>),
    /// An object with declared properties.
    Object(Object),
    /// A reference to a [`NamedType`].
    Ref(String),
    /// `T | null`, however the document spelled it.
    Nullable(Box<Type>),
    /// `oneOf` or `anyOf`, normalised: never empty, never containing `null`.
    Union(Vec<Type>),
    /// `allOf`.
    Every(Vec<Type>),
    /// A construct with no representation, carrying the reason.
    Opaque(String),
}

/// An object with declared properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// The declared properties, sorted by name.
    pub properties: Vec<Property>,
    /// What may appear besides them.
    pub additional: Additional,
}

/// One declared property.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    /// The name as it appears on the wire.
    pub name: String,
    /// Its type.
    pub ty: Type,
    /// Whether the object's `required` array names it.
    pub required: bool,
    /// The property schema's `description`.
    pub description: Option<String>,
    /// Whether the property schema is marked deprecated.
    pub deprecated: bool,
    /// `readOnly`: sent by the server, ignored on the way in.
    pub read_only: bool,
    /// `writeOnly`: accepted on the way in, never sent back.
    pub write_only: bool,
}

/// What an object permits besides its declared properties.
#[derive(Debug, Clone, PartialEq)]
pub enum Additional {
    /// `additionalProperties: false`, or absent.
    Closed,
    /// `additionalProperties: true`.
    Open,
    /// `additionalProperties: <schema>`.
    Typed(Box<Type>),
}

/// One operation.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The function name, in snake case. Emitters case it as they need.
    pub name: String,
    /// The name of the arguments type, when the operation takes any.
    pub params_name: Option<String>,
    /// The HTTP method, upper case.
    pub method: String,
    /// The path template, with `{placeholders}`.
    pub path: String,
    /// The first line of the handler's doc comment.
    pub summary: Option<String>,
    /// The rest of it.
    pub description: Option<String>,
    /// Whether clients should migrate away.
    pub deprecated: bool,
    /// The security schemes the operation requires, sorted and deduplicated.
    pub security: Vec<String>,
    /// Path, query and header parameters, sorted by place then name.
    pub parameters: Vec<Parameter>,
    /// The request body, when it takes one.
    pub body: Option<Body>,
    /// What a successful call yields.
    pub returns: Returns,
    /// The type of a documented failure body, when the document declares one.
    pub problem: Option<Type>,
    /// Documented success responses, for the doc comment.
    pub success: Vec<ResponseCase>,
    /// Documented failure responses, for the doc comment.
    pub failures: Vec<ResponseCase>,
    /// What this operation's description could not carry across.
    pub notes: Vec<String>,
}

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// In the path template.
    Path,
    /// In the query string.
    Query,
    /// In a request header.
    Header,
}

impl Place {
    /// The `in` value this came from.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Place::Path => "path",
            Place::Query => "query",
            Place::Header => "header",
        }
    }
}

/// How an array or object query parameter is spelled on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `style: form, explode: true` — one occurrence per element. The default.
    Form,
    /// `style: form, explode: false` — one occurrence, comma separated.
    FormJoined,
    /// `style: deepObject` — `name[key]=value`.
    Deep,
    /// `style: spaceDelimited`.
    Space,
    /// `style: pipeDelimited`.
    Pipe,
}

impl Style {
    /// The name the generated runtimes use for this style.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Style::Form => "form",
            Style::FormJoined => "formJoined",
            Style::Deep => "deepObject",
            Style::Space => "spaceDelimited",
            Style::Pipe => "pipeDelimited",
        }
    }
}

/// One parameter.
#[derive(Debug, Clone)]
pub struct Parameter {
    /// The wire name.
    pub name: String,
    /// Where it travels.
    pub place: Place,
    /// Whether it must be supplied.
    pub required: bool,
    /// Its description.
    pub description: Option<String>,
    /// Whether it is deprecated.
    pub deprecated: bool,
    /// Its type.
    pub ty: Type,
    /// How a composite value is serialised. Only read for query parameters.
    pub style: Style,
}

/// How a body is carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    /// `application/json`, or any `+json` structured suffix.
    Json,
    /// `application/x-www-form-urlencoded`.
    Form,
    /// `text/*`.
    Text,
    /// `application/octet-stream` and other opaque byte streams.
    Binary,
    /// `multipart/form-data`.
    Multipart,
    /// `text/event-stream`.
    EventStream,
    /// Anything else, keeping its media type.
    Other(String),
}

impl Media {
    /// The media type as it goes into a `content-type` header.
    #[must_use]
    pub fn content_type(&self) -> &str {
        match self {
            Media::Json => "application/json",
            Media::Form => "application/x-www-form-urlencoded",
            Media::Text => "text/plain",
            Media::Binary => "application/octet-stream",
            Media::Multipart => "multipart/form-data",
            Media::EventStream => "text/event-stream",
            Media::Other(name) => name,
        }
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub struct Body {
    /// How it is carried.
    pub media: Media,
    /// Its type.
    pub ty: Type,
    /// Whether the operation refuses a request without one.
    pub required: bool,
    /// Its description.
    pub description: Option<String>,
}

/// A response key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// A concrete code, such as `200`.
    Code(u16),
    /// A wildcard range, such as `2XX`, carried as its leading digit.
    Range(u8),
    /// The `default` response.
    Default,
}

impl Status {
    /// How this reads in a doc comment.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Status::Code(code) => code.to_string(),
            Status::Range(digit) => format!("{digit}XX"),
            Status::Default => "default".to_owned(),
        }
    }

    /// Whether this key describes a failure.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        match self {
            Status::Code(code) => code >= 400,
            Status::Range(digit) => digit >= 4,
            Status::Default => true,
        }
    }
}

/// One documented response.
#[derive(Debug, Clone)]
pub struct ResponseCase {
    /// Which status it answers.
    pub status: Status,
    /// Its description.
    pub description: Option<String>,
    /// How its body is carried, when it has one.
    pub media: Option<Media>,
    /// Its body type, when it has one.
    pub ty: Option<Type>,
}

/// What a successful call yields.
#[derive(Debug, Clone, PartialEq)]
pub enum Returns {
    /// No documented success body.
    Nothing,
    /// A JSON body. `optional` when some success status carries no body at all.
    Json {
        /// The decoded type.
        ty: Type,
        /// Whether a bodiless success is also documented.
        optional: bool,
    },
    /// A `text/*` body.
    Text,
    /// A byte stream.
    Binary,
    /// A media type the generated client will not decode for you, and why.
    ///
    /// The raw response is handed back instead. `text/event-stream` is the case
    /// that matters: a stream is not a value, and pretending otherwise would
    /// mean buffering it whole.
    Raw(String),
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The HTTP methods a path item may carry, in the order they are read.
///
/// Fixed rather than derived from the document so that two documents with the
/// same operations in a different key order produce the same client.
const METHODS: [&str; 8] = [
    "delete", "get", "head", "options", "patch", "post", "put", "trace",
];

/// The media types a request body is looked for in, most preferred first.
const BODY_PREFERENCE: [&str; 5] = [
    "application/json",
    "application/x-www-form-urlencoded",
    "text/plain",
    "application/octet-stream",
    "multipart/form-data",
];

/// JSON Schema keywords this model does not carry across.
///
/// Present in a schema, each adds a note naming it. They constrain a value
/// rather than shape it, so the generated type stays right while the constraint
/// is only enforced by the server — which is where Moso enforces it anyway.
const UNCARRIED: [&str; 8] = [
    "not",
    "if",
    "patternProperties",
    "dependentSchemas",
    "dependentRequired",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
];

impl Api {
    /// Reduce an OpenAPI document to the model.
    ///
    /// # Errors
    /// [`Fault::User`](crate::exit::Fault::User) when the value is not an
    /// OpenAPI object at all, or announces a major version this generator was
    /// not written against. Everything else degrades: an unrepresentable
    /// construct becomes [`Type::Opaque`] and a note.
    pub fn parse(document: &Value) -> Outcome<Self> {
        let root = document.as_object().ok_or_else(|| {
            CliError::user("the OpenAPI document is not a JSON object")
                .with_help("pass --input <file> a document, not an array or a string")
        })?;

        let announced = root.get("openapi").and_then(Value::as_str);
        match announced {
            Some(version) if version.starts_with("3.") => {}
            Some(version) => {
                return Err(CliError::user(format!(
                    "this document announces OpenAPI {version}, and the generator \
                     targets 3.1"
                ))
                .with_help("convert it to 3.1, or generate from a Moso application"));
            }
            None => {
                return Err(
                    CliError::user("the document has no `openapi` version member")
                        .with_help("this is not an OpenAPI document"),
                );
            }
        }

        let mut builder = Builder::new(document);
        builder.declare_schemas();
        builder.build_schemas();
        let operations = builder.operations();

        let info = root.get("info");
        let mut types = builder.types;
        types.sort_by(|left, right| left.name.cmp(&right.name));
        let mut notes = builder.notes;
        notes.sort();
        notes.dedup();

        Ok(Self {
            title: text(info, "title").unwrap_or_else(|| "API".to_owned()),
            version: text(info, "version").unwrap_or_else(|| "0.0.0".to_owned()),
            description: text(info, "description"),
            base_url: base_url(root.get("servers")),
            types,
            operations,
            notes,
        })
    }
}

/// The state one parse carries.
struct Builder<'a> {
    /// The document, for resolving references.
    document: &'a Value,
    /// Names taken by types.
    types_named: NameAllocator,
    /// Names taken by operations, which live in their own namespace.
    operations_named: BTreeSet<String>,
    /// Component schema name to allocated type name.
    schemas: BTreeMap<String, String>,
    /// Everything named so far.
    types: Vec<NamedType>,
    /// What could not be carried across.
    notes: Vec<String>,
}

impl<'a> Builder<'a> {
    /// Start from a document.
    fn new(document: &'a Value) -> Self {
        Self {
            document,
            types_named: NameAllocator::new(),
            operations_named: BTreeSet::new(),
            schemas: BTreeMap::new(),
            types: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Look up a member of the document by JSON pointer-ish path segments.
    fn at(&self, segments: &[&str]) -> Option<&'a Value> {
        let mut current = self.document;
        for segment in segments {
            current = current.get(segment)?;
        }
        Some(current)
    }

    /// Claim a name for every component schema before any of them is read.
    ///
    /// Two passes are needed because a schema may reference one declared after
    /// it, and a forward reference must resolve to the same name the definition
    /// will get.
    fn declare_schemas(&mut self) {
        for name in sorted_keys(self.at(&["components", "schemas"])) {
            let (allocated, note) = self.types_named.allocate(&name);
            if let Some(note) = note {
                self.notes.push(note);
            }
            self.schemas.insert(name, allocated);
        }
    }

    /// Read every component schema into a [`NamedType`].
    fn build_schemas(&mut self) {
        let declared: Vec<(String, String)> = self
            .schemas
            .iter()
            .map(|(schema, allocated)| (schema.clone(), allocated.clone()))
            .collect();

        for (schema, allocated) in declared {
            let Some(value) = self.at(&["components", "schemas", &schema]) else {
                continue;
            };
            let ty = self.schema(value, &allocated, true);
            self.types.push(NamedType {
                name: allocated,
                schema_name: Some(schema),
                description: text(Some(value), "description"),
                deprecated: flag(value, "deprecated"),
                ty,
            });
        }
    }

    /// Register an anonymous shape under a name of its own.
    fn hoist(&mut self, ty: Type, hint: &str, description: Option<String>) -> Type {
        let (name, note) = self.types_named.allocate(hint);
        if let Some(note) = note {
            self.notes.push(note);
        }
        self.types.push(NamedType {
            name: name.clone(),
            schema_name: None,
            description,
            deprecated: false,
            ty,
        });
        Type::Ref(name)
    }

    /// Reduce one schema.
    ///
    /// `hint` is the name an anonymous shape would be hoisted under, and `top`
    /// is set only for a component schema, which already has a name.
    fn schema(&mut self, value: &Value, hint: &str, top: bool) -> Type {
        let ty = self.shape(value, hint);
        if top {
            // A component whose whole shape is `T | null` still needs `T` named:
            // an alias to an optional cannot also declare the union or the enum
            // inside it. `Nullable` becomes `Option<NullableValue>` rather than
            // losing what it was optional *over*.
            return match ty {
                Type::Nullable(inner) if worth_naming(&inner) => {
                    let named = self.hoist(*inner, &format!("{hint}Value"), None);
                    Type::Nullable(Box::new(named))
                }
                other => other,
            };
        }
        if !worth_naming(&ty) {
            return ty;
        }
        let hint = text(Some(value), "title").unwrap_or_else(|| hint.to_owned());
        let description = text(Some(value), "description");
        self.hoist(ty, &hint, description)
    }

    /// Reduce one schema without hoisting it.
    fn shape(&mut self, value: &Value, hint: &str) -> Type {
        let map = match value {
            Value::Bool(true) => return Type::Unknown,
            Value::Bool(false) => {
                return Type::Opaque("the schema is `false`, which matches nothing".to_owned());
            }
            Value::Object(map) => map,
            other => {
                return Type::Opaque(format!(
                    "a schema must be an object or a boolean, and this one is {}",
                    kind_of(other)
                ));
            }
        };

        for keyword in UNCARRIED {
            if map.contains_key(keyword) {
                self.notes.push(format!(
                    "`{keyword}` on `{hint}` is enforced by the server only: the generated \
                     type does not express it"
                ));
            }
        }

        if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
            return self.reference(reference);
        }
        if let Some(members) = map.get("allOf").and_then(Value::as_array) {
            let parts = self.members(members, hint, "AllOf");
            return Type::Every(parts);
        }
        for keyword in ["oneOf", "anyOf"] {
            if let Some(members) = map.get(keyword).and_then(Value::as_array) {
                if keyword == "anyOf" {
                    self.notes.push(format!(
                        "`anyOf` on `{hint}` is generated as a union: a value matching more \
                         than one member is decoded as the first that fits"
                    ));
                }
                let parts = self.members(members, hint, "Variant");
                return union(parts);
            }
        }
        if map.contains_key("prefixItems") {
            return Type::Opaque(format!(
                "`{hint}` is a tuple schema (`prefixItems`), which this generator does not \
                 model"
            ));
        }
        if let Some(values) = map.get("enum").and_then(Value::as_array) {
            return self.enumeration(values.clone(), hint);
        }
        if let Some(value) = map.get("const") {
            return self.enumeration(vec![value.clone()], hint);
        }

        match map.get("type") {
            Some(Value::String(name)) => self.typed(map, name, hint),
            Some(Value::Array(names)) => {
                let parts: Vec<Type> = names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| self.typed(map, name, hint))
                    .collect();
                union(parts)
            }
            Some(other) => Type::Opaque(format!(
                "`type` on `{hint}` is {}, and must be a string or an array of them",
                kind_of(other)
            )),
            None if map.contains_key("properties") || map.contains_key("additionalProperties") => {
                self.object(map, hint)
            }
            None if map.contains_key("items") => self.array(map, hint),
            None => Type::Unknown,
        }
    }

    /// Reduce one named JSON type.
    fn typed(&mut self, map: &serde_json::Map<String, Value>, name: &str, hint: &str) -> Type {
        match name {
            "null" => Type::Null,
            "boolean" => Type::Boolean,
            "integer" => Type::Integer,
            "number" => Type::Number,
            "string" => {
                if map.get("format").and_then(Value::as_str) == Some("binary") {
                    Type::Binary
                } else {
                    Type::Text
                }
            }
            "array" => self.array(map, hint),
            "object" => self.object(map, hint),
            other => Type::Opaque(format!(
                "`{hint}` declares `type: {other}`, which is not a JSON Schema type"
            )),
        }
    }

    /// Reduce an array schema.
    fn array(&mut self, map: &serde_json::Map<String, Value>, hint: &str) -> Type {
        let items = match map.get("items") {
            Some(items) => self.schema(items, &format!("{hint}Item"), false),
            None => Type::Unknown,
        };
        Type::List(Box::new(items))
    }

    /// Reduce an object schema.
    fn object(&mut self, map: &serde_json::Map<String, Value>, hint: &str) -> Type {
        let required: BTreeSet<&str> = map
            .get("required")
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let declared = map.get("properties").and_then(Value::as_object);
        let mut properties = Vec::new();
        if let Some(declared) = declared {
            let mut names: Vec<&String> = declared.keys().collect();
            names.sort();
            for name in names {
                let value = &declared[name];
                let child = format!("{hint}{}", to_pascal(name));
                properties.push(Property {
                    ty: self.schema(value, &child, false),
                    required: required.contains(name.as_str()),
                    description: text(Some(value), "description"),
                    deprecated: flag(value, "deprecated"),
                    read_only: flag(value, "readOnly"),
                    write_only: flag(value, "writeOnly"),
                    name: name.clone(),
                });
            }
        }

        let additional = match map.get("additionalProperties") {
            None | Some(Value::Bool(false)) => Additional::Closed,
            Some(Value::Bool(true)) => Additional::Open,
            Some(schema) => Additional::Typed(Box::new(self.schema(
                schema,
                &format!("{hint}Value"),
                false,
            ))),
        };

        if properties.is_empty() {
            if map.get("additionalProperties") == Some(&Value::Bool(false)) {
                self.notes.push(format!(
                    "`{hint}` declares no properties and forbids others, so it accepts only \
                     `{{}}`: the generated type is an empty map instead"
                ));
            }
            return match additional {
                Additional::Typed(value) => Type::Map(value),
                Additional::Closed | Additional::Open => Type::Map(Box::new(Type::Unknown)),
            };
        }

        Type::Object(Object {
            properties,
            additional,
        })
    }

    /// Reduce an `enum` or `const`.
    fn enumeration(&mut self, values: Vec<Value>, hint: &str) -> Type {
        if values
            .iter()
            .any(|value| !value.is_null() && !is_scalar(value))
        {
            return Type::Opaque(format!(
                "`{hint}` enumerates values that are not scalars, which this generator does \
                 not model"
            ));
        }
        let nullable = values.iter().any(Value::is_null);
        let kept: Vec<Value> = values
            .into_iter()
            .filter(|value| !value.is_null())
            .collect();
        if kept.is_empty() {
            return Type::Null;
        }
        let ty = Type::Enum(kept);
        if nullable {
            Type::Nullable(Box::new(ty))
        } else {
            ty
        }
    }

    /// Reduce the members of a composition keyword.
    fn members(&mut self, members: &[Value], hint: &str, suffix: &str) -> Vec<Type> {
        members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let child = format!("{hint}{suffix}{}", index + 1);
                self.schema(member, &child, false)
            })
            .collect()
    }

    /// Resolve a `$ref`.
    fn reference(&mut self, reference: &str) -> Type {
        let Some(name) = reference.strip_prefix("#/components/schemas/") else {
            return Type::Opaque(format!(
                "`{reference}` is not a reference into `components/schemas`, and this \
                 generator resolves no other kind"
            ));
        };
        let name = unescape_pointer(name);
        match self.schemas.get(&name) {
            Some(allocated) => Type::Ref(allocated.clone()),
            None => Type::Opaque(format!(
                "`{reference}` resolves to nothing: the document declares no such schema"
            )),
        }
    }

    /// Follow a `$ref` one step within the document, for the non-schema
    /// component sections a document may factor out.
    /// Returns an owned value rather than a borrow: the caller goes straight on
    /// to read schemas out of it, which needs `&mut self`, and a borrow of the
    /// document would hold `self` immutably across that call.
    fn resolve(&self, value: &Value, section: &str) -> Option<Value> {
        let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
            return Some(value.clone());
        };
        let prefix = format!("#/components/{section}/");
        let name = reference.strip_prefix(&prefix)?;
        self.at(&["components", section, &unescape_pointer(name)])
            .cloned()
    }

    /// Read every operation in the document.
    fn operations(&mut self) -> Vec<Operation> {
        let mut operations = Vec::new();
        for path in sorted_keys(self.at(&["paths"])) {
            let Some(item) = self.at(&["paths", &path]) else {
                continue;
            };
            let shared = item.get("parameters").cloned();
            for method in METHODS {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                if !operation.is_object() {
                    continue;
                }
                operations.push(self.operation(&path, method, operation, shared.as_ref()));
            }
        }
        operations.sort_by(|left, right| left.name.cmp(&right.name));
        operations
    }

    /// Read one operation.
    fn operation(
        &mut self,
        path: &str,
        method: &str,
        operation: &Value,
        shared: Option<&Value>,
    ) -> Operation {
        let mut notes = Vec::new();
        let name = self.operation_name(operation, method, path);
        let hint = to_pascal(&name);

        let parameters = self.parameters(operation, shared, &hint, &mut notes);
        let body = self.request_body(operation, &hint, &mut notes);
        let (success, failures) = self.responses(operation, &hint, &mut notes);
        let returns = self.returns(&success, &hint, &mut notes);
        let problem = self.problem(&failures, &hint);

        let params_name = if parameters.is_empty() && body.is_none() {
            None
        } else {
            let (allocated, note) = self.types_named.allocate(&format!("{hint}Params"));
            if let Some(note) = note {
                self.notes.push(note);
            }
            Some(allocated)
        };

        let (summary, description) = summary_and_description(operation);

        Operation {
            name,
            params_name,
            method: method.to_uppercase(),
            path: path.to_owned(),
            summary,
            description,
            deprecated: flag(operation, "deprecated"),
            security: security(operation),
            parameters,
            body,
            returns,
            problem,
            success,
            failures,
            notes,
        }
    }

    /// Choose the function name for one operation.
    fn operation_name(&mut self, operation: &Value, method: &str, path: &str) -> String {
        let declared = operation
            .get("operationId")
            .and_then(Value::as_str)
            .map(to_snake)
            .filter(|name| !name.is_empty());
        let base = declared.unwrap_or_else(|| derived_name(method, path));

        if self.operations_named.insert(base.clone()) {
            return base;
        }
        for suffix in 2..1000u32 {
            let candidate = format!("{base}_{suffix}");
            if self.operations_named.insert(candidate.clone()) {
                self.notes.push(format!(
                    "two operations are called `{base}`; the second is generated as \
                     `{candidate}`"
                ));
                return candidate;
            }
        }
        base
    }

    /// Read the parameters of one operation, path-item ones included.
    fn parameters(
        &mut self,
        operation: &Value,
        shared: Option<&Value>,
        hint: &str,
        notes: &mut Vec<String>,
    ) -> Vec<Parameter> {
        let mut found: BTreeMap<(Place, String), Parameter> = BTreeMap::new();
        let lists = [shared, operation.get("parameters")];

        for list in lists.into_iter().flatten() {
            let Some(list) = list.as_array() else {
                continue;
            };
            for entry in list {
                let Some(entry) = self.resolve(entry, "parameters") else {
                    notes.push("a parameter `$ref` resolves to nothing and was skipped".to_owned());
                    continue;
                };
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let place = match entry.get("in").and_then(Value::as_str) {
                    Some("path") => Place::Path,
                    Some("query") => Place::Query,
                    Some("header") => Place::Header,
                    Some("cookie") => {
                        notes.push(format!(
                            "the `{name}` cookie parameter is not an argument: a browser \
                             sends cookies itself, and `fetch` refuses to set the header"
                        ));
                        continue;
                    }
                    _ => continue,
                };
                let child = format!("{hint}{}", to_pascal(name));
                let ty = match entry.get("schema") {
                    Some(schema) => self.schema(schema, &child, false),
                    None => Type::Unknown,
                };
                let required = place == Place::Path || flag(&entry, "required");
                found.insert(
                    (place, name.to_owned()),
                    Parameter {
                        name: name.to_owned(),
                        place,
                        required,
                        description: text(Some(&entry), "description"),
                        deprecated: flag(&entry, "deprecated"),
                        ty,
                        style: style_of(&entry, place, name, notes),
                    },
                );
            }
        }

        found.into_values().collect()
    }

    /// Read the request body of one operation.
    fn request_body(
        &mut self,
        operation: &Value,
        hint: &str,
        notes: &mut Vec<String>,
    ) -> Option<Body> {
        let declared = operation.get("requestBody")?;
        let Some(declared) = self.resolve(declared, "requestBodies") else {
            notes.push("the request body `$ref` resolves to nothing and was skipped".to_owned());
            return None;
        };
        let content = declared.get("content")?.as_object()?;
        let (media_type, entry) = choose_content(content, &BODY_PREFERENCE)?;
        let media = media_of(&media_type);
        if media == Media::Multipart {
            notes.push(format!(
                "`{hint}` takes a `multipart/form-data` body, which is passed through as you \
                 build it rather than typed field by field"
            ));
        }
        let ty = match entry.get("schema") {
            Some(schema) => self.schema(schema, &format!("{hint}Body"), false),
            None => Type::Unknown,
        };
        Some(Body {
            media,
            ty,
            required: flag(&declared, "required"),
            description: text(Some(&declared), "description"),
        })
    }

    /// Read the responses of one operation, split into successes and failures.
    fn responses(
        &mut self,
        operation: &Value,
        hint: &str,
        notes: &mut Vec<String>,
    ) -> (Vec<ResponseCase>, Vec<ResponseCase>) {
        let mut success = Vec::new();
        let mut failures = Vec::new();

        for key in sorted_keys(operation.get("responses")) {
            let Some(status) = parse_status(&key) else {
                notes.push(format!(
                    "the response key `{key}` is neither a status code, an `NXX` range nor \
                     `default`, and was skipped"
                ));
                continue;
            };
            let Some(declared) = operation.get("responses").and_then(|all| all.get(&key)) else {
                continue;
            };
            let Some(declared) = self.resolve(declared, "responses") else {
                notes.push(format!(
                    "the `{key}` response `$ref` resolves to nothing and was skipped"
                ));
                continue;
            };

            let content = declared.get("content").and_then(Value::as_object);
            let chosen = content.and_then(|content| choose_content(content, &BODY_PREFERENCE));
            let (media, ty) = match chosen {
                Some((media_type, entry)) => {
                    let media = media_of(&media_type);
                    let ty = match entry.get("schema") {
                        Some(schema) => {
                            self.schema(schema, &format!("{hint}{}", status_hint(status)), false)
                        }
                        None => Type::Unknown,
                    };
                    (Some(media), Some(ty))
                }
                None => (None, None),
            };

            let case = ResponseCase {
                status,
                description: text(Some(&declared), "description"),
                media,
                ty,
            };
            if status.is_failure() {
                failures.push(case);
            } else {
                success.push(case);
            }
        }

        (success, failures)
    }

    /// Decide what a successful call yields.
    fn returns(
        &mut self,
        success: &[ResponseCase],
        hint: &str,
        notes: &mut Vec<String>,
    ) -> Returns {
        let bodied: Vec<&ResponseCase> =
            success.iter().filter(|case| case.media.is_some()).collect();
        if bodied.is_empty() {
            return Returns::Nothing;
        }

        let media = bodied[0].media.clone().unwrap_or(Media::Json);
        if bodied
            .iter()
            .any(|case| case.media.as_ref() != Some(&media))
        {
            let listed: Vec<String> = bodied
                .iter()
                .filter_map(|case| case.media.as_ref())
                .map(|media| media.content_type().to_owned())
                .collect();
            notes.push(format!(
                "`{hint}` documents more than one response media type ({}), so the raw \
                 response is handed back",
                listed.join(", ")
            ));
            return Returns::Raw(format!("it answers with one of: {}", listed.join(", ")));
        }

        match media {
            Media::Json => {
                let mut kinds: Vec<Type> = Vec::new();
                for case in &bodied {
                    if let Some(ty) = &case.ty
                        && !kinds.contains(ty)
                    {
                        kinds.push(ty.clone());
                    }
                }
                let ty = if kinds.len() > 1 {
                    self.hoist(union(kinds), &format!("{hint}Success"), None)
                } else {
                    kinds.into_iter().next().unwrap_or(Type::Unknown)
                };
                Returns::Json {
                    ty,
                    optional: bodied.len() < success.len(),
                }
            }
            Media::Text => Returns::Text,
            Media::Binary => Returns::Binary,
            Media::EventStream => Returns::Raw(
                "it answers with `text/event-stream`, and a stream is not a value: read \
                 `response.body` yourself"
                    .to_owned(),
            ),
            other => Returns::Raw(format!(
                "it answers with `{}`, which this generator does not decode",
                other.content_type()
            )),
        }
    }

    /// Decide the type of a documented failure body.
    fn problem(&mut self, failures: &[ResponseCase], hint: &str) -> Option<Type> {
        let mut kinds: Vec<Type> = Vec::new();
        for case in failures {
            if case.media != Some(Media::Json) {
                continue;
            }
            if let Some(ty) = &case.ty
                && !kinds.contains(ty)
            {
                kinds.push(ty.clone());
            }
        }
        match kinds.len() {
            0 => None,
            1 => kinds.into_iter().next(),
            _ => Some(self.hoist(union(kinds), &format!("{hint}Failure"), None)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether a shape needs a name of its own.
///
/// Rust cannot spell an object, an enum, a union or an intersection inside a
/// field type; TypeScript can, but a shared name is worth more than an inline
/// literal, and it keeps the two outputs calling the same thing the same word.
fn worth_naming(ty: &Type) -> bool {
    match ty {
        Type::Object(object) => !object.properties.is_empty(),
        Type::Enum(values) => !values.is_empty(),
        Type::Union(members) => members.len() > 1,
        Type::Every(members) => !members.is_empty(),
        _ => false,
    }
}

/// Normalise a union: flatten, deduplicate, and pull `null` out as nullability.
///
/// The point is that both emitters then have exactly one thing to say about
/// optionality. `type: [string, "null"]`, `oneOf: [{type: string}, {type:
/// "null"}]` and a bare `nullable` all arrive here and leave as
/// `Nullable(Text)`.
fn union(members: Vec<Type>) -> Type {
    let mut flat: Vec<Type> = Vec::new();
    let mut nullable = false;

    let mut queue: std::collections::VecDeque<Type> = members.into_iter().collect();
    while let Some(member) = queue.pop_front() {
        match member {
            Type::Null => nullable = true,
            Type::Nullable(inner) => {
                nullable = true;
                queue.push_front(*inner);
            }
            Type::Union(inner) => {
                for (index, part) in inner.into_iter().enumerate() {
                    queue.insert(index, part);
                }
            }
            other => {
                if !flat.contains(&other) {
                    flat.push(other);
                }
            }
        }
    }

    let ty = match flat.len() {
        0 => return Type::Null,
        1 => flat.into_iter().next().unwrap_or(Type::Unknown),
        _ => Type::Union(flat),
    };
    if nullable && ty != Type::Unknown {
        Type::Nullable(Box::new(ty))
    } else {
        ty
    }
}

/// A string member of a JSON object, when it is a non-empty string.
fn text(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|found| !found.is_empty())
        .map(str::to_owned)
}

/// A boolean member, defaulting to false.
fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The keys of a JSON object, sorted.
fn sorted_keys(value: Option<&Value>) -> Vec<String> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

/// Whether a JSON value can be written as a literal in both target languages.
fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// How a JSON value reads in a diagnostic.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Undo RFC 6901 escaping in the fragment of a `$ref`.
fn unescape_pointer(name: &str) -> String {
    name.replace("~1", "/").replace("~0", "~")
}

/// Split `summary` and `description`.
fn summary_and_description(operation: &Value) -> (Option<String>, Option<String>) {
    (
        text(Some(operation), "summary"),
        text(Some(operation), "description"),
    )
}

/// The security scheme names one operation requires, sorted and deduplicated.
fn security(operation: &Value) -> Vec<String> {
    let Some(requirements) = operation.get("security").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names: Vec<String> = requirements
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|requirement| requirement.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The function name for an operation with no `operationId`.
///
/// `GET /users/{id}/posts` becomes `get_users_by_id_posts`, which is the
/// derivation `01-http/14-openapi.md` documents for an undocumented handler.
fn derived_name(method: &str, path: &str) -> String {
    let mut parts = vec![method.to_owned()];
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        let cleaned = segment.trim_start_matches('{').trim_end_matches('}');
        if segment.starts_with('{') {
            parts.push(format!("by_{}", to_snake(cleaned)));
        } else {
            parts.push(to_snake(cleaned));
        }
    }
    let joined = parts.join("_");
    if joined.is_empty() {
        "call".to_owned()
    } else {
        joined
    }
}

/// Read a response key.
fn parse_status(key: &str) -> Option<Status> {
    if key == "default" {
        return Some(Status::Default);
    }
    if let Ok(code) = key.parse::<u16>()
        && (100..=599).contains(&code)
    {
        return Some(Status::Code(code));
    }
    let bytes = key.as_bytes();
    if bytes.len() == 3
        && bytes[0].is_ascii_digit()
        && key[1..].eq_ignore_ascii_case("XX")
        && (1..=5).contains(&(bytes[0] - b'0'))
    {
        return Some(Status::Range(bytes[0] - b'0'));
    }
    None
}

/// The name hint for a schema found under one response status.
fn status_hint(status: Status) -> String {
    match status {
        Status::Code(code) => format!("Response{code}"),
        Status::Range(digit) => format!("Response{digit}xx"),
        Status::Default => "ResponseDefault".to_owned(),
    }
}

/// Classify a media type.
fn media_of(media_type: &str) -> Media {
    let base = media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase();
    if base == "application/json" || base.ends_with("+json") {
        return Media::Json;
    }
    match base.as_str() {
        "application/x-www-form-urlencoded" => Media::Form,
        "multipart/form-data" => Media::Multipart,
        "text/event-stream" => Media::EventStream,
        "application/octet-stream" => Media::Binary,
        other if other.starts_with("text/") => Media::Text,
        other => Media::Other(other.to_owned()),
    }
}

/// Pick one entry out of a `content` map.
///
/// Preference first, then the sorted remainder, so that a document offering
/// several representations always produces the same client.
fn choose_content(
    content: &serde_json::Map<String, Value>,
    preference: &[&str],
) -> Option<(String, Value)> {
    for wanted in preference {
        for (media_type, entry) in content {
            if media_of(media_type) == media_of(wanted) {
                return Some((media_type.clone(), entry.clone()));
            }
        }
    }
    let mut keys: Vec<&String> = content.keys().collect();
    keys.sort();
    keys.first()
        .map(|key| ((*key).clone(), content[*key].clone()))
}

/// Read `style` and `explode` off a parameter.
fn style_of(entry: &Value, place: Place, name: &str, notes: &mut Vec<String>) -> Style {
    let declared = entry.get("style").and_then(Value::as_str);
    let explode = entry.get("explode").and_then(Value::as_bool);
    match declared {
        None | Some("form") => {
            if explode == Some(false) {
                Style::FormJoined
            } else {
                Style::Form
            }
        }
        Some("deepObject") => Style::Deep,
        Some("spaceDelimited") => Style::Space,
        Some("pipeDelimited") => Style::Pipe,
        Some("simple") => Style::Form,
        Some(other) => {
            notes.push(format!(
                "the `{name}` {} parameter uses `style: {other}`, which this generator does \
                 not implement: it is sent as `form`",
                place.as_str()
            ));
            Style::Form
        }
    }
}

/// The first server URL, with its variables' defaults substituted.
fn base_url(servers: Option<&Value>) -> Option<String> {
    let first = servers?.as_array()?.first()?;
    let mut url = first.get("url").and_then(Value::as_str)?.to_owned();
    if let Some(variables) = first.get("variables").and_then(Value::as_object) {
        let mut names: Vec<&String> = variables.keys().collect();
        names.sort();
        for name in names {
            if let Some(default) = variables[name].get("default").and_then(Value::as_str) {
                url = url.replace(&format!("{{{name}}}"), default);
            }
        }
    }
    Some(url.trim_end_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Wrap a schema in the smallest document that carries it.
    fn document(schemas: Value) -> Value {
        json!({
            "openapi": "3.1.1",
            "info": {"title": "Test", "version": "1.0.0"},
            "paths": {},
            "components": {"schemas": schemas},
        })
    }

    fn parse(schemas: Value) -> Api {
        Api::parse(&document(schemas)).expect("the document parses")
    }

    fn named<'a>(api: &'a Api, name: &str) -> &'a NamedType {
        api.types
            .iter()
            .find(|found| found.name == name)
            .unwrap_or_else(|| panic!("no type called {name} in {:?}", names(api)))
    }

    fn names(api: &Api) -> Vec<&str> {
        api.types.iter().map(|found| found.name.as_str()).collect()
    }

    // ── the document as a whole ───────────────────────────────────────────

    #[test]
    fn a_document_that_is_not_openapi_is_refused_rather_than_guessed_at() {
        let error = Api::parse(&json!({"paths": {}})).expect_err("refused");
        assert_eq!(error.fault, crate::exit::Fault::User);
        let error = Api::parse(&json!({"openapi": "2.0"})).expect_err("refused");
        assert!(error.message.contains("3.1"));
        assert!(Api::parse(&json!([])).is_err());
    }

    #[test]
    fn the_server_url_loses_its_trailing_slash_and_gains_its_defaults() {
        let mut document = document(json!({}));
        document["servers"] = json!([{
            "url": "https://{region}.example.com/v1/",
            "variables": {"region": {"default": "eu"}},
        }]);
        let api = Api::parse(&document).expect("parses");
        assert_eq!(api.base_url.as_deref(), Some("https://eu.example.com/v1"));
    }

    // ── types ─────────────────────────────────────────────────────────────

    #[test]
    fn a_nullable_type_array_becomes_one_nullable_type() {
        let api = parse(json!({"Name": {"type": ["string", "null"]}}));
        assert_eq!(named(&api, "Name").ty, Type::Nullable(Box::new(Type::Text)));
    }

    #[test]
    fn a_one_of_with_a_null_member_is_the_same_thing() {
        let api = parse(json!({
            "Name": {"oneOf": [{"type": "string"}, {"type": "null"}]},
        }));
        assert_eq!(named(&api, "Name").ty, Type::Nullable(Box::new(Type::Text)));
    }

    #[test]
    fn a_union_of_several_members_keeps_them_in_document_order() {
        let api = parse(json!({
            "Either": {"oneOf": [{"type": "integer"}, {"type": "string"}]},
        }));
        assert_eq!(
            named(&api, "Either").ty,
            Type::Union(vec![Type::Integer, Type::Text])
        );
    }

    #[test]
    fn a_reference_resolves_to_the_allocated_name() {
        let api = parse(json!({
            "Post": {"type": "object", "properties": {"id": {"type": "string"}}},
            "Wrapper": {
                "type": "object",
                "properties": {"post": {"$ref": "#/components/schemas/Post"}},
                "required": ["post"],
            },
        }));
        let Type::Object(object) = &named(&api, "Wrapper").ty else {
            panic!("expected an object");
        };
        assert_eq!(object.properties[0].ty, Type::Ref("Post".to_owned()));
        assert!(object.properties[0].required);
    }

    #[test]
    fn a_dangling_reference_is_opaque_and_says_so() {
        let api = parse(json!({
            "Wrapper": {
                "type": "object",
                "properties": {"post": {"$ref": "#/components/schemas/Nope"}},
            },
        }));
        let Type::Object(object) = &named(&api, "Wrapper").ty else {
            panic!("expected an object");
        };
        let Type::Opaque(reason) = &object.properties[0].ty else {
            panic!("expected an opaque type, got {:?}", object.properties[0].ty);
        };
        assert!(reason.contains("resolves to nothing"), "{reason}");
    }

    #[test]
    fn an_external_reference_is_opaque_rather_than_fetched() {
        let api = parse(json!({
            "Wrapper": {
                "type": "object",
                "properties": {"post": {"$ref": "https://example.com/Post.json"}},
            },
        }));
        let Type::Object(object) = &named(&api, "Wrapper").ty else {
            panic!("expected an object");
        };
        assert!(matches!(object.properties[0].ty, Type::Opaque(_)));
    }

    #[test]
    fn an_inline_object_is_hoisted_under_its_title() {
        let api = parse(json!({
            "Problem": {
                "type": "object",
                "properties": {
                    "errors": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "title": "FieldError",
                            "properties": {"code": {"type": "string"}},
                            "required": ["code"],
                        },
                    },
                },
            },
        }));
        assert!(names(&api).contains(&"FieldError"), "{:?}", names(&api));
        let Type::Object(object) = &named(&api, "Problem").ty else {
            panic!("expected an object");
        };
        assert_eq!(
            object.properties[0].ty,
            Type::List(Box::new(Type::Ref("FieldError".to_owned())))
        );
    }

    #[test]
    fn an_untitled_inline_object_is_hoisted_under_its_path() {
        let api = parse(json!({
            "Post": {
                "type": "object",
                "properties": {"author": {"type": "object", "properties": {"name": {"type": "string"}}}},
            },
        }));
        assert!(names(&api).contains(&"PostAuthor"), "{:?}", names(&api));
    }

    #[test]
    fn properties_are_sorted_so_two_orderings_of_one_document_agree() {
        let api = parse(json!({
            "Post": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "author": {"type": "string"},
                },
            },
        }));
        let Type::Object(object) = &named(&api, "Post").ty else {
            panic!("expected an object");
        };
        let ordered: Vec<&str> = object
            .properties
            .iter()
            .map(|property| property.name.as_str())
            .collect();
        assert_eq!(ordered, ["author", "body", "title"]);
    }

    #[test]
    fn additional_properties_are_carried_three_ways() {
        let api = parse(json!({
            "Closed": {"type": "object", "properties": {"a": {"type": "string"}}},
            "Open": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": true,
            },
            "Typed": {
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": {"type": "integer"},
            },
            "Free": {"type": "object", "additionalProperties": {"type": "integer"}},
        }));
        let additional = |name: &str| match &named(&api, name).ty {
            Type::Object(object) => object.additional.clone(),
            other => panic!("{name} is {other:?}"),
        };
        assert_eq!(additional("Closed"), Additional::Closed);
        assert_eq!(additional("Open"), Additional::Open);
        assert_eq!(
            additional("Typed"),
            Additional::Typed(Box::new(Type::Integer))
        );
        assert_eq!(named(&api, "Free").ty, Type::Map(Box::new(Type::Integer)));
    }

    #[test]
    fn an_enum_becomes_a_closed_set_and_a_nullable_one_keeps_its_nullability() {
        let api = parse(json!({
            "State": {"type": "string", "enum": ["draft", "published"]},
            "Maybe": {"enum": ["a", null]},
            "One": {"const": "only"},
        }));
        assert_eq!(
            named(&api, "State").ty,
            Type::Enum(vec![json!("draft"), json!("published")])
        );
        // A component that is `T | null` keeps its nullability, and `T` gets a
        // name: an alias to an optional cannot also declare the enum inside it.
        assert_eq!(
            named(&api, "Maybe").ty,
            Type::Nullable(Box::new(Type::Ref("MaybeValue".to_owned())))
        );
        assert_eq!(named(&api, "MaybeValue").ty, Type::Enum(vec![json!("a")]));
        assert_eq!(named(&api, "One").ty, Type::Enum(vec![json!("only")]));
    }

    #[test]
    fn all_of_is_carried_as_an_intersection() {
        let api = parse(json!({
            "Base": {"type": "object", "properties": {"id": {"type": "string"}}},
            "Extended": {
                "allOf": [
                    {"$ref": "#/components/schemas/Base"},
                    {"type": "object", "properties": {"extra": {"type": "string"}}},
                ],
            },
        }));
        let Type::Every(members) = &named(&api, "Extended").ty else {
            panic!("expected an intersection");
        };
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], Type::Ref("Base".to_owned()));
    }

    #[test]
    fn binary_is_distinguished_from_text() {
        let api = parse(json!({
            "Upload": {"type": "string", "format": "binary"},
            "Word": {"type": "string", "format": "uuid"},
        }));
        assert_eq!(named(&api, "Upload").ty, Type::Binary);
        assert_eq!(named(&api, "Word").ty, Type::Text);
    }

    #[test]
    fn a_schema_named_after_the_runtime_is_renamed_rather_than_shadowing_it() {
        // `Blob` is a DOM global the TypeScript runtime names, and `Result` is
        // in Rust's prelude; a generated type of either name would break the
        // file it lands in.
        let api = parse(json!({
            "Blob": {"type": "string"},
            "Result": {"type": "string"},
            "Problem": {"type": "object", "properties": {"title": {"type": "string"}}},
        }));
        assert!(names(&api).contains(&"Blob2"), "{:?}", names(&api));
        assert!(names(&api).contains(&"Result2"), "{:?}", names(&api));
        // But the schema Moso really publishes keeps the name it was given.
        assert!(names(&api).contains(&"Problem"), "{:?}", names(&api));
        assert_eq!(
            api.notes.len(),
            2,
            "each rename is reported: {:?}",
            api.notes
        );
    }

    #[test]
    fn a_tuple_schema_is_refused_by_name_rather_than_mangled() {
        let api = parse(json!({
            "Pair": {"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}]},
        }));
        let Type::Opaque(reason) = &named(&api, "Pair").ty else {
            panic!("expected an opaque type");
        };
        assert!(reason.contains("prefixItems"), "{reason}");
    }

    #[test]
    fn a_constraint_this_model_cannot_carry_becomes_a_note_not_a_silence() {
        let api = parse(json!({
            "Odd": {"type": "object", "patternProperties": {"^x-": {"type": "string"}}},
        }));
        assert!(
            api.notes
                .iter()
                .any(|note| note.contains("patternProperties")),
            "{:?}",
            api.notes
        );
    }

    #[test]
    fn a_name_collision_renames_the_second_and_reports_it() {
        let mut allocator = NameAllocator::new();
        assert_eq!(allocator.allocate("post_out").0, "PostOut");
        let (name, note) = allocator.allocate("PostOut");
        assert_eq!(name, "PostOut2");
        assert!(note.is_some_and(|note| note.contains("PostOut2")));
        // And the runtime's own names are taken before anything is read.
        assert_eq!(allocator.allocate("client").0, "Client2");
    }

    // ── operations ────────────────────────────────────────────────────────

    fn with_paths(paths: Value) -> Api {
        let mut document = document(json!({}));
        document["paths"] = paths;
        Api::parse(&document).expect("parses")
    }

    #[test]
    fn an_operation_id_becomes_the_function_name() {
        let api = with_paths(json!({
            "/posts": {"get": {"operationId": "posts_list", "responses": {}}},
        }));
        assert_eq!(api.operations[0].name, "posts_list");
        assert_eq!(api.operations[0].method, "GET");
    }

    #[test]
    fn an_operation_without_an_id_gets_one_derived_from_its_route() {
        assert_eq!(
            derived_name("get", "/users/{id}/posts"),
            "get_users_by_id_posts"
        );
        assert_eq!(derived_name("post", "/"), "post");
        let api = with_paths(json!({"/users/{id}": {"get": {"responses": {}}}}));
        assert_eq!(api.operations[0].name, "get_users_by_id");
    }

    #[test]
    fn parameters_are_merged_from_the_path_item_and_sorted_by_place() {
        let api = with_paths(json!({
            "/posts/{id}": {
                "parameters": [{"name": "id", "in": "path", "schema": {"type": "string"}}],
                "get": {
                    "operationId": "show",
                    "parameters": [
                        {"name": "verbose", "in": "query", "schema": {"type": "boolean"}},
                        {"name": "x-tenant", "in": "header", "schema": {"type": "string"}},
                    ],
                    "responses": {},
                },
            },
        }));
        let parameters = &api.operations[0].parameters;
        assert_eq!(parameters.len(), 3);
        assert_eq!(parameters[0].place, Place::Path);
        assert!(
            parameters[0].required,
            "a path parameter is always required"
        );
        assert_eq!(parameters[1].place, Place::Query);
        assert_eq!(parameters[2].name, "x-tenant");
    }

    #[test]
    fn a_cookie_parameter_is_left_out_with_the_reason_recorded() {
        let api = with_paths(json!({
            "/posts": {"get": {
                "operationId": "list",
                "parameters": [{"name": "session", "in": "cookie", "schema": {"type": "string"}}],
                "responses": {},
            }},
        }));
        assert!(api.operations[0].parameters.is_empty());
        assert!(
            api.operations[0]
                .notes
                .iter()
                .any(|note| note.contains("session")),
            "{:?}",
            api.operations[0].notes
        );
    }

    #[test]
    fn the_request_body_prefers_json_over_everything_else() {
        let api = with_paths(json!({
            "/posts": {"post": {
                "operationId": "create",
                "requestBody": {
                    "required": true,
                    "content": {
                        "text/plain": {"schema": {"type": "string"}},
                        "application/json": {"schema": {"type": "object", "properties": {"a": {"type": "string"}}}},
                    },
                },
                "responses": {},
            }},
        }));
        let body = api.operations[0].body.as_ref().expect("a body");
        assert_eq!(body.media, Media::Json);
        assert!(body.required);
    }

    #[test]
    fn responses_are_split_and_the_success_type_is_the_one_json_body() {
        let api = with_paths(json!({
            "/posts": {"get": {
                "operationId": "list",
                "responses": {
                    "200": {"description": "ok", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Nope"}}}},
                    "404": {"description": "gone", "content": {"application/problem+json": {"schema": {"type": "object", "properties": {"title": {"type": "string"}}}}}},
                },
            }},
        }));
        let operation = &api.operations[0];
        assert_eq!(operation.success.len(), 1);
        assert_eq!(operation.failures.len(), 1);
        assert!(matches!(operation.returns, Returns::Json { .. }));
        assert!(operation.problem.is_some(), "the 404 body is typed");
    }

    #[test]
    fn a_204_alone_returns_nothing_and_a_mixed_pair_returns_an_optional() {
        let api = with_paths(json!({
            "/a": {"delete": {"operationId": "destroy", "responses": {"204": {"description": "gone"}}}},
            "/b": {"get": {"operationId": "show", "responses": {
                "200": {"description": "ok", "content": {"application/json": {"schema": {"type": "string"}}}},
                "304": {"description": "cached"},
            }}},
        }));
        let destroy = api
            .operations
            .iter()
            .find(|op| op.name == "destroy")
            .expect("op");
        assert_eq!(destroy.returns, Returns::Nothing);
        let show = api
            .operations
            .iter()
            .find(|op| op.name == "show")
            .expect("op");
        assert_eq!(
            show.returns,
            Returns::Json {
                ty: Type::Text,
                optional: true
            }
        );
    }

    #[test]
    fn an_event_stream_hands_back_the_raw_response_and_says_why() {
        let api = with_paths(json!({
            "/events": {"get": {"operationId": "watch", "responses": {
                "200": {"description": "ok", "content": {"text/event-stream": {"schema": {"type": "string"}}}},
            }}},
        }));
        let Returns::Raw(reason) = &api.operations[0].returns else {
            panic!("expected a raw return, got {:?}", api.operations[0].returns);
        };
        assert!(reason.contains("stream is not a value"), "{reason}");
    }

    #[test]
    fn two_operations_with_one_id_are_both_generated_under_distinct_names() {
        let api = with_paths(json!({
            "/a": {"get": {"operationId": "same", "responses": {}}},
            "/b": {"get": {"operationId": "same", "responses": {}}},
        }));
        let called: Vec<&str> = api.operations.iter().map(|op| op.name.as_str()).collect();
        assert_eq!(called, ["same", "same_2"]);
        assert!(api.notes.iter().any(|note| note.contains("same_2")));
    }

    #[test]
    fn the_security_schemes_an_operation_needs_are_sorted_and_deduplicated() {
        let api = with_paths(json!({
            "/a": {"get": {
                "operationId": "guarded",
                "security": [{"session": []}, {"api_key": []}, {"session": []}],
                "responses": {},
            }},
        }));
        assert_eq!(api.operations[0].security, ["api_key", "session"]);
    }

    #[test]
    fn every_status_key_form_is_read_and_a_bad_one_is_reported() {
        assert_eq!(parse_status("200"), Some(Status::Code(200)));
        assert_eq!(parse_status("2XX"), Some(Status::Range(2)));
        assert_eq!(parse_status("default"), Some(Status::Default));
        assert_eq!(parse_status("nope"), None);
        assert_eq!(parse_status("99"), None);
        assert!(Status::Code(404).is_failure());
        assert!(!Status::Code(201).is_failure());
        assert!(Status::Default.is_failure());
    }

    #[test]
    fn media_types_are_classified_by_their_structured_suffix() {
        assert_eq!(media_of("application/problem+json"), Media::Json);
        assert_eq!(media_of("application/json; charset=utf-8"), Media::Json);
        assert_eq!(media_of("text/html"), Media::Text);
        assert_eq!(media_of("text/event-stream"), Media::EventStream);
        assert_eq!(media_of("application/octet-stream"), Media::Binary);
        assert_eq!(media_of("image/png"), Media::Other("image/png".to_owned()));
    }

    #[test]
    fn a_query_style_that_is_not_implemented_is_reported_rather_than_assumed() {
        let mut notes = Vec::new();
        let entry = json!({"style": "matrix"});
        assert_eq!(
            style_of(&entry, Place::Query, "id", &mut notes),
            Style::Form
        );
        assert!(notes[0].contains("matrix"), "{notes:?}");

        let mut ignored = Vec::new();
        assert_eq!(
            style_of(&json!({"explode": false}), Place::Query, "q", &mut ignored),
            Style::FormJoined
        );
        assert_eq!(
            style_of(
                &json!({"style": "deepObject"}),
                Place::Query,
                "q",
                &mut ignored
            ),
            Style::Deep
        );
        assert!(ignored.is_empty());
    }

    #[test]
    fn parsing_the_same_document_twice_produces_the_same_model() {
        let source = document(json!({
            "Post": {"type": "object", "properties": {"b": {"type": "string"}, "a": {"type": "integer"}}},
        }));
        let first = Api::parse(&source).expect("parses");
        let second = Api::parse(&source).expect("parses");
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
    }
}
