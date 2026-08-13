//! Reading rustdoc's JSON output, which is how two of the gates see the public
//! API without parsing Rust.
//!
//! # Why JSON and not source
//!
//! `check-sealed` has to answer "does a foreign path appear in a *public*
//! signature", and `check-diagnostics` has to answer "which traits are public".
//! Both questions are about the API after `pub use`, `#[doc(hidden)]`, private
//! modules and re-exports have had their say, and the only thing that knows the
//! answer is rustdoc. Grepping the source would be wrong in both directions: it
//! would miss a leak that arrives through a re-export and flag a type in a
//! private module that nobody can name.
//!
//! # Why it works on stable
//!
//! `--output-format json` is a nightly rustdoc option, and Moso is a
//! stable-only project (`rustc 1.97.1`). `RUSTC_BOOTSTRAP=1` is what
//! `cargo-expand` and `cargo-public-api` use for the same reason: it unlocks
//! `-Z` on a stable toolchain. It is not a supported interface, so
//! [`Doc::produce`] pins nothing on the *contents* of the JSON beyond what it
//! reads dynamically, and [`Doc::format_version`] is reported in every gate's
//! output so a format change shows up as a number rather than as a silent
//! false pass.
//!
//! The alternative — requiring a nightly toolchain to run the gates — was
//! rejected because the gates then cannot run in the same CI job as the build,
//! and a gate that is awkward to run is a gate that gets skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::bail;
use crate::util::{Cmd, Error, Result};

/// The lowest rustdoc JSON format version this module has been read against.
///
/// A different version is not an error — every field this module touches has
/// been stable for a long time — but it is printed, so that a gate which starts
/// passing for the wrong reason has a visible cause.
///
/// ```
/// assert!(xtask::rustdoc::KNOWN_FORMAT_VERSION >= 45);
/// ```
pub const KNOWN_FORMAT_VERSION: u64 = 57;

/// One crate's rustdoc JSON.
///
/// ```
/// use xtask::rustdoc::Doc;
///
/// let json = r#"{"root":"1","crate_version":null,"includes_private":false,
///   "index":{"1":{"id":"1","crate_id":0,"name":"demo","attrs":[],
///                 "inner":{"module":{"is_crate":true,"items":[]}}}},
///   "paths":{"1":{"crate_id":0,"path":["demo"],"kind":"module"}},
///   "external_crates":{},"format_version":57}"#;
/// let doc = Doc::from_json("demo", json)?;
/// assert_eq!(doc.format_version(), 57);
/// assert_eq!(doc.crate_name(), "demo");
/// # Ok::<(), xtask::util::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Doc {
    crate_name: String,
    root: Value,
}

impl Doc {
    /// Runs `cargo rustdoc` for `package` and reads the JSON it writes.
    ///
    /// `target_dir` selects where the artefacts go; passing `None` uses the
    /// workspace's own `target/`, which is what makes this a two-second
    /// operation rather than a full rebuild.
    ///
    /// ```no_run
    /// use xtask::rustdoc::Doc;
    ///
    /// let root = xtask::util::workspace_root()?;
    /// let doc = Doc::produce(&root, "moso-schema", None)?;
    /// assert!(!doc.local_traits().is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    pub fn produce(root: &Path, package: &str, target_dir: Option<&Path>) -> Result<Self> {
        let mut cmd = Cmd::cargo()
            .cwd(root)
            .env("RUSTC_BOOTSTRAP", "1")
            .env("RUSTDOCFLAGS", "-Z unstable-options --output-format json")
            .args(["rustdoc", "--package", package, "--lib"]);
        if let Some(dir) = target_dir {
            cmd = cmd.args(["--target-dir", &dir.display().to_string()]);
        }
        let output = cmd.capture()?;
        if !output.ok() {
            bail!(
                "cannot produce rustdoc JSON for {package}\n{}\n    the command was: {}",
                crate::util::indent(&output.stderr_tail(15)),
                cmd.rendered()
            );
        }

        let doc_dir = match target_dir {
            Some(dir) => dir.join("doc"),
            None => root.join("target").join("doc"),
        };
        let file = doc_dir.join(format!("{}.json", package.replace('-', "_")));
        let json = std::fs::read_to_string(&file).map_err(|error| {
            Error::new(format!(
                "rustdoc reported success but {} is not readable: {error}",
                file.display()
            ))
        })?;
        Self::from_json(package, &json)
    }

    /// Parses rustdoc JSON that is already in hand.
    ///
    /// ```
    /// use xtask::rustdoc::Doc;
    ///
    /// let error = Doc::from_json("demo", "{}").expect_err("no index");
    /// assert!(error.to_string().contains("index"));
    /// ```
    pub fn from_json(package: &str, json: &str) -> Result<Self> {
        let root: Value = serde_json::from_str(json)
            .map_err(|error| Error::from(error).with_context(format!("rustdoc JSON {package}")))?;
        if !root.get("index").is_some_and(Value::is_object) {
            bail!("rustdoc JSON for {package} has no `index` object");
        }
        if !root.get("paths").is_some_and(Value::is_object) {
            bail!("rustdoc JSON for {package} has no `paths` object");
        }
        Ok(Self {
            crate_name: package.to_owned(),
            root,
        })
    }

    /// The package name this document describes.
    ///
    /// ```no_run
    /// # let doc = xtask::rustdoc::Doc::from_json("x", "{\"index\":{},\"paths\":{}}").unwrap();
    /// assert_eq!(doc.crate_name(), "x");
    /// ```
    #[must_use]
    pub fn crate_name(&self) -> &str {
        &self.crate_name
    }

    /// The `format_version` field, or `0` when absent.
    ///
    /// ```
    /// # use xtask::rustdoc::Doc;
    /// let doc = Doc::from_json("x", "{\"index\":{},\"paths\":{}}")?;
    /// assert_eq!(doc.format_version(), 0);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn format_version(&self) -> u64 {
        self.root
            .get("format_version")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    }

    /// Every documented item, keyed by id.
    ///
    /// ```
    /// # use xtask::rustdoc::Doc;
    /// let doc = Doc::from_json("x", "{\"index\":{},\"paths\":{}}")?;
    /// assert!(doc.index().is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn index(&self) -> &Map<String, Value> {
        self.root
            .get("index")
            .and_then(Value::as_object)
            .expect("checked in from_json")
    }

    /// The summary of every item any signature refers to, keyed by id.
    ///
    /// ```
    /// # use xtask::rustdoc::Doc;
    /// let doc = Doc::from_json("x", "{\"index\":{},\"paths\":{}}")?;
    /// assert!(doc.paths().is_empty());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn paths(&self) -> &Map<String, Value> {
        self.root
            .get("paths")
            .and_then(Value::as_object)
            .expect("checked in from_json")
    }

    /// The item with this id.
    ///
    /// ```
    /// # use xtask::rustdoc::Doc;
    /// let doc = Doc::from_json("x", "{\"index\":{},\"paths\":{}}")?;
    /// assert!(doc.item("7").is_none());
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&Value> {
        self.index().get(id)
    }

    /// Resolves an id to the crate that defines it and the path it is known by.
    ///
    /// ```
    /// use xtask::rustdoc::Doc;
    ///
    /// let json = r#"{"index":{},"paths":{"9":{"crate_id":3,"path":["sea_query","SelectStatement"],
    ///   "kind":"struct"}},"external_crates":{"3":{"name":"sea_query"}},"format_version":57}"#;
    /// let doc = Doc::from_json("moso-sql", json)?;
    /// let owner = doc.owner_of("9").expect("a known id");
    /// assert_eq!(owner.crate_name, "sea_query");
    /// assert_eq!(owner.path, "sea_query::SelectStatement");
    /// assert!(!owner.local);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn owner_of(&self, id: &str) -> Option<Owner> {
        let entry = self.paths().get(id)?;
        let crate_id = entry.get("crate_id").and_then(Value::as_u64).unwrap_or(0);
        let segments: Vec<String> = entry
            .get("path")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let crate_name = if crate_id == 0 {
            segments
                .first()
                .cloned()
                .unwrap_or_else(|| self.crate_name.replace('-', "_"))
        } else {
            self.external_crate_name(crate_id)
                .unwrap_or_else(|| format!("crate#{crate_id}"))
        };
        Some(Owner {
            crate_name,
            path: segments.join("::"),
            kind: entry
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("item")
                .to_owned(),
            local: crate_id == 0,
        })
    }

    fn external_crate_name(&self, crate_id: u64) -> Option<String> {
        self.root
            .get("external_crates")?
            .as_object()?
            .get(&crate_id.to_string())?
            .get("name")?
            .as_str()
            .map(str::to_owned)
    }

    /// Every trait this crate defines and rustdoc kept, with its public path.
    ///
    /// Traits arriving through `pub use` from another crate are not listed: they
    /// belong to the crate that defined them, and that is where their
    /// diagnostic has to live.
    ///
    /// ```
    /// use xtask::rustdoc::Doc;
    ///
    /// let json = r#"{"index":{"4":{"id":"4","crate_id":0,"name":"Entity","attrs":[],
    ///     "span":{"filename":"src/lib.rs","begin":[10,1]},
    ///     "inner":{"trait":{"items":[]}}}},
    ///   "paths":{"4":{"crate_id":0,"path":["moso_orm","Entity"],"kind":"trait"}},
    ///   "external_crates":{},"format_version":57}"#;
    /// let doc = Doc::from_json("moso-orm", json)?;
    /// let traits = doc.local_traits();
    /// assert_eq!(traits.len(), 1);
    /// assert_eq!(traits[0].path, "moso_orm::Entity");
    /// assert_eq!(traits[0].location().as_deref(), Some("src/lib.rs:10"));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn local_traits(&self) -> Vec<TraitDef> {
        let mut traits: Vec<TraitDef> = Vec::new();
        for (id, entry) in self.paths() {
            if entry.get("kind").and_then(Value::as_str) != Some("trait") {
                continue;
            }
            if entry.get("crate_id").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            let Some(item) = self.item(id) else { continue };
            let path = self
                .owner_of(id)
                .map(|owner| owner.path)
                .unwrap_or_else(|| "?".to_owned());
            traits.push(TraitDef {
                id: id.clone(),
                path,
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned(),
                attrs: rendered_attrs(item),
                file: span_file(item),
                line: span_line(item),
            });
        }
        traits.sort_by(|a, b| a.path.cmp(&b.path));
        traits
    }

    /// Every blanket implementation in this crate: an `impl<T> Trait for T`
    /// whose self type *is* one of the impl's own type parameters.
    ///
    /// These are the impls `docs/04-devex/41-diagnostics.md` requires
    /// `#[diagnostic::do_not_recommend]` on, because they are what the compiler
    /// otherwise offers as a fix.
    ///
    /// ```
    /// use xtask::rustdoc::Doc;
    ///
    /// let json = r#"{"index":{"5":{"id":"5","crate_id":0,"name":null,"attrs":[],
    ///     "span":{"filename":"src/router.rs","begin":[651,1]},
    ///     "inner":{"impl":{"generics":{"params":[{"name":"G","kind":{"type":{}}}],
    ///        "where_predicates":[]},
    ///       "trait":{"path":"DynGuard","id":"1","args":null},
    ///       "for":{"generic":"G"},"items":[]}}}},
    ///   "paths":{},"external_crates":{},"format_version":57}"#;
    /// let doc = Doc::from_json("moso-core", json)?;
    /// let blankets = doc.blanket_impls();
    /// assert_eq!(blankets.len(), 1);
    /// assert_eq!(blankets[0].trait_name, "DynGuard");
    /// assert!(!blankets[0].has_do_not_recommend);
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn blanket_impls(&self) -> Vec<BlanketImpl> {
        let mut found = Vec::new();
        for item in self.index().values() {
            let Some(imp) = item.pointer("/inner/impl") else {
                continue;
            };
            let Some(self_param) = imp
                .get("for")
                .and_then(|ty| ty.get("generic"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let is_own_param = imp
                .pointer("/generics/params")
                .and_then(Value::as_array)
                .is_some_and(|params| {
                    params
                        .iter()
                        .any(|param| param.get("name").and_then(Value::as_str) == Some(self_param))
                });
            if !is_own_param {
                continue;
            }
            let trait_name = imp
                .get("trait")
                .and_then(|t| t.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("<inherent>")
                .to_owned();
            let attrs = rendered_attrs(item);
            found.push(BlanketImpl {
                trait_name,
                self_param: self_param.to_owned(),
                has_do_not_recommend: attrs.contains("DoNotRecommend")
                    || attrs.contains("do_not_recommend"),
                file: span_file(item),
                line: span_line(item),
            });
        }
        found.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        found
    }

    /// Maps every item that belongs to an `impl` block back to that block.
    ///
    /// `check-sealed` needs this to tell a signature its author chose from one
    /// a foreign trait dictated: the body of `impl Serialize for Sql` has to
    /// name `serde::Serializer`, and holding that against the crate would make
    /// the gate unusable.
    ///
    /// ```
    /// use xtask::rustdoc::Doc;
    ///
    /// let json = r#"{"index":{"5":{"id":"5","crate_id":0,"name":null,"attrs":[],
    ///     "inner":{"impl":{"generics":{"params":[],"where_predicates":[]},
    ///       "trait":{"path":"Serialize","id":"9","args":null},
    ///       "for":{"resolved_path":{"path":"Sql","id":"2","args":null}},
    ///       "items":["6"]}}},
    ///   "6":{"id":"6","crate_id":0,"name":"serialize","attrs":[],
    ///        "inner":{"function":{"sig":{"inputs":[],"output":null}}}}},
    ///   "paths":{},"external_crates":{},"format_version":57}"#;
    /// let doc = Doc::from_json("moso-sql", json)?;
    /// let owners = doc.impl_owners();
    /// assert_eq!(owners.get("6").map(|o| o.trait_name.as_deref()), Some(Some("Serialize")));
    /// # Ok::<(), xtask::util::Error>(())
    /// ```
    #[must_use]
    pub fn impl_owners(&self) -> BTreeMap<String, ImplOwner> {
        let mut owners = BTreeMap::new();
        for (id, item) in self.index() {
            let Some(imp) = item.pointer("/inner/impl") else {
                continue;
            };
            let trait_name = imp
                .get("trait")
                .and_then(|t| t.get("path"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(children) = imp.get("items").and_then(Value::as_array) else {
                continue;
            };
            for child in children {
                if let Some(child_id) = id_key(child) {
                    owners.insert(
                        child_id,
                        ImplOwner {
                            impl_id: id.clone(),
                            trait_name: trait_name.clone(),
                        },
                    );
                }
            }
        }
        owners
    }
}

/// Where an id is defined.
///
/// ```
/// use xtask::rustdoc::Owner;
///
/// let owner = Owner { crate_name: "std".into(), path: "std::string::String".into(),
///     kind: "struct".into(), local: false };
/// assert_eq!(owner.crate_name, "std");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The defining crate, with underscores as rustdoc spells it.
    pub crate_name: String,
    /// The full path, `::`-joined.
    pub path: String,
    /// `struct`, `trait`, `enum`, and so on.
    pub kind: String,
    /// Whether the definition is in the crate being documented.
    pub local: bool,
}

/// A trait defined by the crate under inspection.
///
/// ```
/// use xtask::rustdoc::TraitDef;
///
/// let def = TraitDef { id: "1".into(), path: "moso_core::Handler".into(),
///     name: "Handler".into(), attrs: "#[attr = OnUnimplemented { .. }]".into(),
///     file: Some("src/handler.rs".into()), line: Some(42) };
/// assert!(def.has_on_unimplemented());
/// assert_eq!(def.location().as_deref(), Some("src/handler.rs:42"));
/// ```
#[derive(Clone, Debug)]
pub struct TraitDef {
    /// The rustdoc id, for looking the item up again.
    pub id: String,
    /// The `::`-joined public path.
    pub path: String,
    /// The trait's own name.
    pub name: String,
    /// Every attribute rustdoc kept, rendered as one string.
    pub attrs: String,
    /// The file the trait is declared in, relative to the workspace root.
    pub file: Option<String>,
    /// The line the declaration starts on.
    pub line: Option<u64>,
}

impl TraitDef {
    /// Whether the trait carries `#[diagnostic::on_unimplemented]`.
    ///
    /// rustdoc renders the attribute in its parsed form,
    /// `#[attr = OnUnimplemented { .. }]`, rather than as written; both
    /// spellings are accepted so the check survives a rendering change.
    ///
    /// ```
    /// # use xtask::rustdoc::TraitDef;
    /// # fn def(attrs: &str) -> TraitDef { TraitDef { id: "1".into(), path: "p".into(),
    /// #     name: "T".into(), attrs: attrs.into(), file: None, line: None } }
    /// assert!(def("#[attr = OnUnimplemented {}]").has_on_unimplemented());
    /// assert!(def("#[diagnostic::on_unimplemented(message = \"x\")]").has_on_unimplemented());
    /// assert!(!def("#[must_use]").has_on_unimplemented());
    /// ```
    #[must_use]
    pub fn has_on_unimplemented(&self) -> bool {
        self.attrs.contains("OnUnimplemented") || self.attrs.contains("on_unimplemented")
    }

    /// `file:line`, when rustdoc recorded a span.
    ///
    /// ```
    /// # use xtask::rustdoc::TraitDef;
    /// let def = TraitDef { id: "1".into(), path: "p".into(), name: "T".into(),
    ///     attrs: String::new(), file: None, line: None };
    /// assert!(def.location().is_none());
    /// ```
    #[must_use]
    pub fn location(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        Some(match self.line {
            Some(line) => format!("{file}:{line}"),
            None => file.clone(),
        })
    }
}

/// A blanket implementation and whether it is marked.
///
/// ```
/// use xtask::rustdoc::BlanketImpl;
///
/// let imp = BlanketImpl { trait_name: "Handler".into(), self_param: "F".into(),
///     has_do_not_recommend: true, file: Some("src/handler.rs".into()), line: Some(300) };
/// assert!(imp.has_do_not_recommend);
/// ```
#[derive(Clone, Debug)]
pub struct BlanketImpl {
    /// The trait being implemented, or `<inherent>`.
    pub trait_name: String,
    /// The name of the type parameter the impl is `for`.
    pub self_param: String,
    /// Whether `#[diagnostic::do_not_recommend]` is present.
    pub has_do_not_recommend: bool,
    /// The file the impl is in.
    pub file: Option<String>,
    /// The line it starts on.
    pub line: Option<u64>,
}

/// The `impl` block an associated item belongs to.
///
/// ```
/// use xtask::rustdoc::ImplOwner;
///
/// let owner = ImplOwner { impl_id: "5".into(), trait_name: Some("Serialize".into()) };
/// assert!(owner.is_trait_impl());
/// ```
#[derive(Clone, Debug)]
pub struct ImplOwner {
    /// The id of the `impl` item.
    pub impl_id: String,
    /// The trait being implemented, if any.
    pub trait_name: Option<String>,
}

impl ImplOwner {
    /// Whether this is a trait impl rather than an inherent one.
    ///
    /// ```
    /// # use xtask::rustdoc::ImplOwner;
    /// let inherent = ImplOwner { impl_id: "5".into(), trait_name: None };
    /// assert!(!inherent.is_trait_impl());
    /// ```
    #[must_use]
    pub fn is_trait_impl(&self) -> bool {
        self.trait_name.is_some()
    }
}

/// A reference to a named item found inside a type.
///
/// ```
/// use xtask::rustdoc::PathRef;
///
/// let reference = PathRef { id: "9".into(), printed: "SelectStatement".into() };
/// assert_eq!(reference.printed, "SelectStatement");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRef {
    /// The referenced item's rustdoc id.
    pub id: String,
    /// How rustdoc printed the path at the use site, which is what the user
    /// reads in the signature.
    pub printed: String,
}

/// Collects every named item a type — or a bundle of generics, or a whole
/// signature — refers to.
///
/// The rule is structural and therefore survives new type variants: rustdoc
/// spells every reference to a named item as an object carrying the printed path
/// (`path`, or `source` on a re-export) next to the target's `id`, whatever the
/// surrounding variant is called. Anything else in the tree (tuples, slices,
/// `impl Trait`, lifetimes, primitives) is walked through.
///
/// ```
/// use xtask::rustdoc::path_refs;
///
/// let ty: serde_json::Value = serde_json::from_str(r#"
///   {"borrowed_ref":{"lifetime":null,"is_mutable":false,
///     "type":{"resolved_path":{"path":"sea_query::SelectStatement","id":"9","args":
///       {"angle_bracketed":{"args":[{"type":{"resolved_path":
///         {"path":"Alias","id":"12","args":null}}}],"constraints":[]}}}}}}"#)?;
/// let refs = path_refs(&ty);
/// assert_eq!(refs.len(), 2, "the type and its generic argument");
/// assert_eq!(refs[0].printed, "sea_query::SelectStatement");
///
/// // A re-export names the target under `source`.
/// let reexport = serde_json::json!({"source": "sea_query::Value", "name": "Value",
///     "id": 9, "is_glob": false});
/// assert_eq!(path_refs(&reexport)[0].printed, "sea_query::Value");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn path_refs(value: &Value) -> Vec<PathRef> {
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

fn walk(value: &Value, out: &mut Vec<PathRef>) {
    match value {
        Value::Object(map) => {
            let printed = map
                .get("path")
                .or_else(|| map.get("source"))
                .and_then(Value::as_str);
            if let (Some(printed), Some(id)) = (printed, map.get("id"))
                && let Some(id) = id_key(id)
            {
                out.push(PathRef {
                    id,
                    printed: printed.to_owned(),
                });
            }
            for nested in map.values() {
                walk(nested, out);
            }
        }
        Value::Array(items) => {
            for nested in items {
                walk(nested, out);
            }
        }
        _ => {}
    }
}

/// Normalises a rustdoc id — a number in recent formats, a string in older ones
/// — to the string the `index` and `paths` maps are keyed by.
///
/// ```
/// use xtask::rustdoc::id_key;
///
/// assert_eq!(id_key(&serde_json::json!(517)).as_deref(), Some("517"));
/// assert_eq!(id_key(&serde_json::json!("0:1:2")).as_deref(), Some("0:1:2"));
/// assert_eq!(id_key(&serde_json::json!(null)), None);
/// ```
#[must_use]
pub fn id_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Every attribute on an item, rendered as one searchable string.
///
/// ```
/// use xtask::rustdoc::rendered_attrs;
///
/// let item = serde_json::json!({"attrs": [{"other": "#[attr = DoNotRecommend]"}]});
/// assert!(rendered_attrs(&item).contains("DoNotRecommend"));
///
/// // Older formats render attributes as plain strings; the whole array is
/// // stringified either way, because the only question asked of it is whether a
/// // particular attribute name appears.
/// let older = serde_json::json!({"attrs": ["#[must_use]"]});
/// assert!(rendered_attrs(&older).contains("must_use"));
///
/// assert_eq!(rendered_attrs(&serde_json::json!({})), "");
/// ```
#[must_use]
pub fn rendered_attrs(item: &Value) -> String {
    item.get("attrs")
        .map(|attrs| attrs.to_string())
        .unwrap_or_default()
}

/// The file an item's span points at.
///
/// ```
/// let item = serde_json::json!({"span": {"filename": "src/lib.rs", "begin": [3, 1]}});
/// assert_eq!(xtask::rustdoc::span_file(&item).as_deref(), Some("src/lib.rs"));
/// ```
#[must_use]
pub fn span_file(item: &Value) -> Option<String> {
    item.pointer("/span/filename")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The line an item's span starts on.
///
/// ```
/// let item = serde_json::json!({"span": {"filename": "src/lib.rs", "begin": [3, 1]}});
/// assert_eq!(xtask::rustdoc::span_line(&item), Some(3));
/// ```
#[must_use]
pub fn span_line(item: &Value) -> Option<u64> {
    item.pointer("/span/begin/0").and_then(Value::as_u64)
}

/// The directory rustdoc JSON is written to for a given target directory.
///
/// ```
/// use std::path::Path;
///
/// let dir = xtask::rustdoc::doc_dir(Path::new("/w"), None);
/// assert_eq!(dir, Path::new("/w/target/doc"));
/// ```
#[must_use]
pub fn doc_dir(root: &Path, target_dir: Option<&Path>) -> PathBuf {
    match target_dir {
        Some(dir) => dir.join("doc"),
        None => root.join("target").join("doc"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r##"{
      "root": "1",
      "index": {
        "1": {"id":"1","crate_id":0,"name":"moso_sql","attrs":[],
              "inner":{"module":{"is_crate":true,"items":["2","4"]}}},
        "2": {"id":"2","crate_id":0,"name":"Select","attrs":[],
              "span":{"filename":"crates/moso-sql/src/lib.rs","begin":[10,1]},
              "inner":{"struct":{"kind":{"plain":{"fields":["3"]}}}}},
        "3": {"id":"3","crate_id":0,"name":"inner","attrs":[],
              "inner":{"struct_field":{"resolved_path":{"path":"sea_query::SelectStatement",
                                                        "id":"90","args":null}}}},
        "4": {"id":"4","crate_id":0,"name":"Dialect","attrs":[
                {"other":"#[attr = OnUnimplemented {directive: Directive {}}]"}],
              "span":{"filename":"crates/moso-sql/src/lib.rs","begin":[30,1]},
              "inner":{"trait":{"items":[]}}}
      },
      "paths": {
        "2": {"crate_id":0,"path":["moso_sql","Select"],"kind":"struct"},
        "4": {"crate_id":0,"path":["moso_sql","Dialect"],"kind":"trait"},
        "90": {"crate_id":7,"path":["sea_query","SelectStatement"],"kind":"struct"}
      },
      "external_crates": {"7": {"name":"sea_query"}},
      "format_version": 57
    }"##;

    fn doc() -> Doc {
        Doc::from_json("moso-sql", DOC).expect("valid rustdoc JSON")
    }

    #[test]
    fn local_traits_are_found_with_their_paths_and_spans() {
        let traits = doc().local_traits();
        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].path, "moso_sql::Dialect");
        assert!(traits[0].has_on_unimplemented());
        assert_eq!(
            traits[0].location().as_deref(),
            Some("crates/moso-sql/src/lib.rs:30")
        );
    }

    #[test]
    fn a_foreign_id_resolves_to_the_crate_that_defines_it() {
        let owner = doc().owner_of("90").expect("in paths");
        assert_eq!(owner.crate_name, "sea_query");
        assert_eq!(owner.path, "sea_query::SelectStatement");
        assert!(!owner.local);
    }

    #[test]
    fn a_local_id_resolves_to_this_crate() {
        let owner = doc().owner_of("2").expect("in paths");
        assert_eq!(owner.crate_name, "moso_sql");
        assert!(owner.local);
    }

    #[test]
    fn an_unknown_id_has_no_owner() {
        assert!(doc().owner_of("404").is_none());
    }

    #[test]
    fn path_refs_finds_the_reference_inside_a_field_type() {
        let field = doc().item("3").expect("the field").clone();
        let refs = path_refs(field.pointer("/inner/struct_field").expect("a type"));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "90");
    }

    #[test]
    fn path_refs_walks_generic_arguments_and_bare_types_alike() {
        let ty = serde_json::json!({
            "resolved_path": {"path": "Vec", "id": "1", "args": {"angle_bracketed": {
                "args": [{"type": {"resolved_path": {"path": "Row", "id": "2", "args": null}}}],
                "constraints": []}}}
        });
        let refs = path_refs(&ty);
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"]);
    }

    #[test]
    fn path_refs_ignores_a_path_without_an_id() {
        // Span filenames are strings under a `filename` key, never `path` +
        // `id`, but a defensive check costs nothing.
        let value = serde_json::json!({"path": "not/a/type"});
        assert!(path_refs(&value).is_empty());
    }

    #[test]
    fn rejecting_json_that_is_not_a_rustdoc_document() {
        assert!(Doc::from_json("x", "[]").is_err());
        assert!(Doc::from_json("x", r#"{"index":{}}"#).is_err());
    }
}
