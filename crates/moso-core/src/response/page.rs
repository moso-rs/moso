//! `Page<T>` — the pagination envelope, decided once.
//!
//! Every API needs pagination and every team invents a different envelope, so
//! clients written against two services in the same company parse two shapes.
//! Moso ships one.
//!
//! ```json
//! {
//!   "items": [ … ],
//!   "next_cursor": "eyJpZCI6…",
//!   "prev_cursor": null,
//!   "total": 1042
//! }
//! ```
//!
//! # Why cursors are the default
//!
//! Offset pagination is wrong at scale in two ways that matter: `OFFSET 100000`
//! makes the database walk a hundred thousand rows, and a row inserted between
//! two requests shifts every subsequent page, so a client scanning the list
//! silently skips records. A cursor encodes the sort key of the last row seen,
//! which costs an index seek and cannot skip.
//!
//! [`Page::from_offset`] exists for the cases where a UI genuinely needs page
//! numbers. It carries `page` and `per_page` in the envelope instead of a
//! cursor — because pretending otherwise would just make people build their own.
//!
//! # Every member past `items` is optional, and each says something
//!
//! One envelope covers both styles, and which members are present *is* the
//! signal for which style is in use:
//!
//! ```text
//! cursor pagination   items + next_cursor? + prev_cursor?
//! offset pagination   items + total + page + per_page
//! the whole set       items
//! ```
//!
//! So a client that sees no `next_cursor` on a full page is not looking at the
//! last page; it is looking at an offset page, and `page`/`per_page`/`total`
//! tell it where it is and how far it has to go.
//!
//! These are the *response*'s members. The matching **request** parameters are
//! not this type's to declare: a handler receives `page` and `per_page` through
//! its own `Query<T>`, and that signature is the one home for what the operation
//! accepts. A response type that also contributed request parameters would be a
//! second, competing description of the same operation.
//!
//! # `total` is optional on purpose
//!
//! Counting is a second query and, on a large filtered table, the expensive one.
//! It is present only when the request asked for it — and always for an offset
//! page, because a page number without a page count is not usable.

use std::borrow::Cow;

use http::StatusCode;
use moso_openapi::OperationBuilder;
use moso_schema::json_schema::{
    ArrayBuilder, NumberBuilder, ObjectBuilder, SchemaGenerator, SchemaNode,
};
use moso_schema::{Cursor, Schema, Validate, ValidationCtx, ValidationErrors, generic_schema_name};
use serde::{Deserialize, Serialize};

use crate::Response;
use crate::error::Result;
use crate::response::{Describe, IntoResponse, describe_json, json_response};

/// One page of results.
///
/// ```
/// use moso::prelude::*;
/// use moso::schema::Cursor;
///
/// /// A post, as the API returns one.
/// #[derive(Schema)]
/// pub struct PostOut {
///     /// URL-safe identifier.
///     pub slug: Slug,
/// }
///
/// /// List posts.
/// #[endpoint]
/// async fn list(Query(_q): Query<()>) -> Result<Page<PostOut>> {
///     Ok(Page::new(vec![PostOut { slug: Slug::from_title("hello").unwrap() }]))
/// }
/// # fn main() {
/// // Cursor pagination: ask for one row more than fits, and let `from_items` decide.
/// let rows: Vec<u64> = vec![1, 2, 3, 4];
/// let page = Page::from_items(rows, 3, |last| Cursor::from_bytes(last.to_be_bytes()));
/// assert_eq!(page.len(), 3);
/// assert!(page.next_cursor.is_some());
///
/// // Offset pagination carries the page it is, and no cursors.
/// let offset = Page::from_offset(vec![1, 2], 1, 2, 57);
/// assert_eq!(offset.total, Some(57));
/// assert_eq!((offset.page, offset.per_page), (Some(1), Some(2)));
/// assert!(offset.next_cursor.is_none());
/// # }
/// ```
///
/// The envelope is `{ "items": [...], "next_cursor": ..., "total": ... }`, and it
/// is the same shape for every listing in the API, which is what lets a client
/// write one pagination helper.
///
/// # Why this is `#[non_exhaustive]`
///
/// `page` and `per_page` were added to a type that already had public fields, and
/// adding a public field to a struct a downstream crate can build with a `Page {
/// .. }` literal is a breaking change — every such literal stops compiling. Making
/// the struct `#[non_exhaustive]` closes struct-literal construction to other
/// crates, so a future field is an additive, non-breaking change; the fields stay
/// `pub` for reading and pattern-matching. Construct a `Page` through
/// [`Page::new`], [`Page::from_items`], [`Page::from_offset`] or the `with_*`
/// builders — never a literal — and nothing in the workspace does otherwise. This
/// note is the whole record of the decision; it needed no RFC because it only
/// forecloses a construction path Moso never intended callers to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Page<T> {
    /// The rows, in the order the query produced them.
    pub items: Vec<T>,
    /// The cursor that fetches the next page, absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    /// The cursor that fetches the previous page, absent on the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<Cursor>,
    /// The total number of matching rows, present only when it was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// The 1-based page number this is, on an offset-paginated listing.
    ///
    /// Present exactly when [`Page::from_offset`] or [`Page::with_offset`] built
    /// the page, and absent for every cursor-paginated one — so the envelope of
    /// a cursor page is byte-identical to what it was before this member
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    /// The page size the listing was asked for, on an offset-paginated listing.
    ///
    /// With [`total`](Page::total) it gives a client the page count, which is
    /// the number a page-number UI actually renders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_page: Option<u32>,
}

impl<T> Page<T> {
    /// A page with no cursors and no total — the whole result set.
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            next_cursor: None,
            prev_cursor: None,
            total: None,
            page: None,
            per_page: None,
        }
    }

    /// An empty page.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Set the cursor for the next page.
    pub fn with_next(mut self, cursor: Cursor) -> Self {
        self.next_cursor = Some(cursor);
        self
    }

    /// Set the cursor for the previous page.
    pub fn with_prev(mut self, cursor: Cursor) -> Self {
        self.prev_cursor = Some(cursor);
        self
    }

    /// Set the total row count.
    pub fn with_total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    /// Record which offset page this is.
    ///
    /// The response half of offset pagination: the client asked for `page` of
    /// `per_page` rows, and the envelope says so, which is what lets a page-
    /// number UI render "page 3 of 42" from one response instead of
    /// remembering what it sent.
    ///
    /// ```
    /// use moso::response::Page;
    ///
    /// let page = Page::new(vec![1u32, 2]).with_total(57).with_offset(3, 2);
    /// assert_eq!((page.page, page.per_page, page.total), (Some(3), Some(2), Some(57)));
    /// ```
    ///
    /// # Panics
    ///
    /// Debug builds only, when `per_page` is zero or the page holds more rows
    /// than fit in it — both of which mean the caller has paired the wrong two
    /// numbers, and both of which would otherwise reach a client as a page
    /// count it cannot make sense of.
    pub fn with_offset(mut self, page: u32, per_page: u32) -> Self {
        debug_assert!(per_page > 0, "an offset page holds at least one row");
        debug_assert!(
            self.items.len() <= per_page as usize,
            "an offset page of {per_page} was given {} rows",
            self.items.len()
        );
        self.page = Some(page);
        self.per_page = Some(per_page);
        self
    }

    /// Convert each row, keeping every envelope member.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            next_cursor: self.next_cursor,
            prev_cursor: self.prev_cursor,
            total: self.total,
            page: self.page,
            per_page: self.per_page,
        }
    }

    /// Convert each row fallibly, short-circuiting on the first failure.
    ///
    /// The shape a projection uses: `page.try_map(PostOut::from_loaded)?`.
    pub fn try_map<U>(self, f: impl FnMut(T) -> Result<U>) -> Result<Page<U>> {
        Ok(Page {
            items: self.items.into_iter().map(f).collect::<Result<Vec<U>>>()?,
            next_cursor: self.next_cursor,
            prev_cursor: self.prev_cursor,
            total: self.total,
            page: self.page,
            per_page: self.per_page,
        })
    }

    /// A page from a `limit + 1` query, with the next cursor derived from the
    /// last row that fits.
    ///
    /// The shape every cursor-paginated query wants: ask the database for one
    /// row more than the page holds, and the presence of that row is what says
    /// there is a next page. Counting is not needed and a second query is not
    /// issued.
    ///
    /// ```
    /// use moso::response::Page;
    /// use moso::schema::Cursor;
    ///
    /// // The query asked for `limit + 1` rows and got them all back …
    /// let rows: Vec<u64> = vec![1, 2, 3, 4];
    /// let page = Page::from_items(rows, 3, |last| Cursor::from_bytes(last.to_be_bytes()));
    ///
    /// // … so the page holds `limit` of them and knows where to resume.
    /// assert_eq!(page.len(), 3);
    /// assert_eq!(page.next_cursor.as_ref().map(Cursor::as_bytes), Some(&3_u64.to_be_bytes()[..]));
    ///
    /// // A short read is the last page, and costs no cursor.
    /// let last = Page::from_items(vec![1, 2], 3, |_| unreachable!("not called"));
    /// assert!(last.next_cursor.is_none());
    /// ```
    ///
    /// `items` is truncated to `limit`; `cursor_for` is called only when it was
    /// longer, so a last page costs nothing.
    pub fn from_items(
        mut items: Vec<T>,
        limit: usize,
        cursor_for: impl FnOnce(&T) -> Cursor,
    ) -> Self {
        let has_more = items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let next_cursor = match items.last() {
            Some(last) if has_more => Some(cursor_for(last)),
            _ => None,
        };
        Self {
            items,
            next_cursor,
            prev_cursor: None,
            total: None,
            page: None,
            per_page: None,
        }
    }

    /// A page built from an offset query.
    ///
    /// Carries the total, the page number, the page size and **no cursors**, so
    /// a client cannot mistake one pagination style for the other halfway
    /// through a scan: the absence of `next_cursor` on a non-final page is the
    /// signal to keep using `page`/`per_page`, and those two are right there in
    /// the envelope to keep using.
    ///
    /// `page` and `per_page` are echoes of what the request asked for, which is
    /// what makes a page-number UI renderable from one response. They are
    /// *response* members: the request parameters of the same name belong on
    /// the handler's own `Query<T>`, which is where the document reads them
    /// from.
    ///
    /// ```
    /// use moso::response::Page;
    ///
    /// let page = Page::from_offset(vec!["a", "b"], 3, 2, 57);
    /// assert_eq!((page.page, page.per_page, page.total), (Some(3), Some(2), Some(57)));
    /// assert!(page.next_cursor.is_none(), "offset pages never carry a cursor");
    /// ```
    ///
    /// # Panics
    ///
    /// Debug builds only, on the two ways of getting the pair wrong. See
    /// [`Page::with_offset`].
    pub fn from_offset(items: Vec<T>, page: u32, per_page: u32, total: u64) -> Self {
        Self::new(items)
            .with_total(total)
            .with_offset(page, per_page)
    }

    /// How many rows this page holds.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the page is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> FromIterator<T> for Page<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Page::new(iter.into_iter().collect())
    }
}

/// A page validates its rows, so a projection that produced an invalid DTO is
/// caught by the same machinery that checks an incoming body.
///
/// The envelope itself has nothing to check: the cursors are opaque and the
/// total is whatever the count said.
impl<T: Validate> Validate for Page<T> {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors> {
        let mut errors = ValidationErrors::new();
        moso_schema::checks::check_each_nested(&self.items, "/items", ctx, &mut errors);
        errors.into_result()
    }
}

impl<T: Schema> Schema for Page<T> {
    fn schema_name() -> Cow<'static, str> {
        generic_schema_name("Page", &[T::schema_name()])
    }

    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode {
        let items = generator.subschema_for::<T>();
        let cursor = generator.subschema_for::<Cursor>();

        ObjectBuilder::named(Self::schema_name())
            .description("One page of results, in Moso's standard pagination envelope.")
            .property(
                "items",
                ArrayBuilder::new()
                    .items(items)
                    .description("The rows, in the order the query produced them.")
                    .build(),
                true,
            )
            .property(
                "next_cursor",
                cursor.clone().with_description(
                    "Pass back as `cursor` to fetch the next page. Absent on the last page.",
                ),
                false,
            )
            .property(
                "prev_cursor",
                cursor.with_description(
                    "Pass back as `cursor` to fetch the previous page. Absent on the first.",
                ),
                false,
            )
            .property(
                "total",
                NumberBuilder::integer()
                    .minimum(0u64)
                    .description(
                        "The number of matching rows. Present only when the request asked for \
                         a count, because counting is a second query.",
                    )
                    .build(),
                false,
            )
            .property(
                "page",
                NumberBuilder::integer()
                    .minimum(1u64)
                    .description(
                        "The 1-based page number this is. Present only on an offset-paginated \
                         listing, which is also the one that carries no cursors.",
                    )
                    .build(),
                false,
            )
            .property(
                "per_page",
                NumberBuilder::integer()
                    .minimum(1u64)
                    .description(
                        "The page size the listing was asked for. With `total` it gives the \
                         page count.",
                    )
                    .build(),
                false,
            )
            .build()
    }

    const HAS_CONSTRAINTS: bool = T::HAS_CONSTRAINTS;
}

impl<T: Schema> IntoResponse for Page<T> {
    fn into_response(self) -> Response {
        json_response(StatusCode::OK, &self)
    }
}

impl<T: Schema> Describe for Page<T> {
    fn describe(op: &mut OperationBuilder) {
        describe_json::<Page<T>>(op, 200);
    }
}

/// RFC 8288 `Link` header values for a page.
///
/// Emitted alongside the JSON envelope so a client that prefers headers — a
/// crawler, a shell script — does not have to parse the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageLinks {
    /// The URL of the next page.
    pub next: Option<String>,
    /// The URL of the previous page.
    pub prev: Option<String>,
    /// The URL of the first page.
    pub first: Option<String>,
}

/// The query parameter a cursor travels in, on the way back.
pub const CURSOR_PARAM: &str = "cursor";

impl PageLinks {
    /// Build the links for `page`, relative to the request URI.
    ///
    /// Every other query parameter is preserved — a filtered listing stays
    /// filtered across pages, which is the bug this avoids — and only `cursor`
    /// is rewritten. `first` is the same request with no cursor at all, so it
    /// is always available.
    pub fn for_page<T>(page: &Page<T>, request_uri: &http::Uri) -> Self {
        Self {
            next: page
                .next_cursor
                .as_ref()
                .map(|cursor| with_cursor(request_uri, Some(&cursor.encode()))),
            prev: page
                .prev_cursor
                .as_ref()
                .map(|cursor| with_cursor(request_uri, Some(&cursor.encode()))),
            first: Some(with_cursor(request_uri, None)),
        }
    }

    /// Whether there is anything to send.
    pub fn is_empty(&self) -> bool {
        self.next.is_none() && self.prev.is_none() && self.first.is_none()
    }

    /// Render as one `Link` header value, or `None` when there are no links.
    pub fn to_header(&self) -> Option<http::HeaderValue> {
        let mut value = String::new();
        for (url, rel) in [
            (self.next.as_deref(), "next"),
            (self.prev.as_deref(), "prev"),
            (self.first.as_deref(), "first"),
        ] {
            let Some(url) = url else { continue };
            if !value.is_empty() {
                value.push_str(", ");
            }
            value.push('<');
            value.push_str(url);
            value.push_str(">; rel=\"");
            value.push_str(rel);
            value.push('"');
        }
        if value.is_empty() {
            return None;
        }
        // A URL built from a `Uri` and a percent-encoded query cannot contain a
        // control character, so this only fails if the caller hand-built one.
        http::HeaderValue::from_str(&value).ok()
    }
}

/// `request_uri` with its `cursor` parameter replaced, or removed for `None`.
fn with_cursor(request_uri: &http::Uri, cursor: Option<&str>) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    for (key, value) in form_urlencoded::parse(request_uri.query().unwrap_or_default().as_bytes()) {
        if key == CURSOR_PARAM {
            continue;
        }
        query.append_pair(&key, &value);
    }
    if let Some(cursor) = cursor {
        query.append_pair(CURSOR_PARAM, cursor);
    }
    let query = query.finish();

    let mut url = String::with_capacity(request_uri.path().len() + query.len() + 16);
    if let (Some(scheme), Some(authority)) = (request_uri.scheme_str(), request_uri.authority()) {
        url.push_str(scheme);
        url.push_str("://");
        url.push_str(authority.as_str());
    }
    url.push_str(request_uri.path());
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::tests::described;
    use serde_json::json;

    fn cursor(bytes: &[u8]) -> Cursor {
        Cursor::from_bytes(bytes)
    }

    #[test]
    fn an_empty_page_has_no_cursors() {
        let page: Page<u32> = Page::empty();
        assert!(page.is_empty());
        assert!(page.next_cursor.is_none());
        assert!(page.total.is_none());
        assert!(Page::<u32>::default().is_empty());
    }

    #[test]
    fn map_preserves_the_envelope() {
        let page = Page::new(vec![1u32, 2, 3]).with_total(3);
        let mapped = page.map(|n| n * 2);
        assert_eq!(mapped.items, vec![2, 4, 6]);
        assert_eq!(mapped.total, Some(3));
    }

    #[test]
    fn collecting_builds_a_page() {
        let page: Page<u8> = (0u8..3).collect();
        assert_eq!(page.len(), 3);
    }

    #[test]
    fn try_map_short_circuits_but_keeps_the_envelope_on_success() {
        let page = Page::new(vec![1u32, 2, 3])
            .with_total(9)
            .with_next(cursor(b"n"))
            .with_prev(cursor(b"p"));

        let ok = page.clone().try_map(|n| Ok(n * 2)).expect("all convert");
        assert_eq!(ok.items, vec![2, 4, 6]);
        assert_eq!(ok.total, Some(9));
        assert_eq!(ok.next_cursor, Some(cursor(b"n")));
        assert_eq!(ok.prev_cursor, Some(cursor(b"p")));

        let err = page
            .try_map(|n| {
                if n == 2 {
                    Err(crate::Error::internal_msg("nope"))
                } else {
                    Ok(n)
                }
            })
            .expect_err("the second row fails");
        assert!(err.is_server_error());
    }

    #[test]
    fn from_items_derives_the_next_cursor_from_the_overflow_row() {
        // limit + 1 rows: the extra one is dropped and proves there is more.
        let page = Page::from_items(vec![1u32, 2, 3, 4], 3, |last| cursor(&[*last as u8]));
        assert_eq!(page.items, vec![1, 2, 3]);
        assert_eq!(page.next_cursor, Some(cursor(&[3])));

        // Exactly `limit` rows is the last page, and costs no cursor.
        let page = Page::from_items(vec![1u32, 2, 3], 3, |_| panic!("must not be called"));
        assert_eq!(page.items, vec![1, 2, 3]);
        assert!(page.next_cursor.is_none());

        // And an empty result is an empty page.
        let page = Page::from_items(Vec::<u32>::new(), 3, |_| panic!("must not be called"));
        assert!(page.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn from_offset_carries_the_page_it_is_and_never_a_cursor() {
        let page = Page::from_offset(vec![1u32, 2], 3, 2, 57);
        assert_eq!(page.total, Some(57));
        assert_eq!(page.page, Some(3));
        assert_eq!(page.per_page, Some(2));
        assert!(page.next_cursor.is_none());
        assert!(page.prev_cursor.is_none());

        // The two arguments reach the wire, which is the point of taking them.
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            json!({"items": [1, 2], "total": 57, "page": 3, "per_page": 2})
        );
    }

    #[test]
    fn a_cursor_page_serialises_exactly_as_it_did_before_the_offset_members() {
        // The offset members are omitted rather than null, so adding them left
        // every cursor-paginated response byte-identical.
        let page = Page::from_items(vec![1u32, 2, 3, 4], 3, |last| cursor(&[*last as u8]));
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            json!({"items": [1, 2, 3], "next_cursor": "Aw"})
        );
    }

    #[test]
    fn the_offset_members_survive_a_projection() {
        let page = Page::from_offset(vec![1u32, 2], 3, 2, 57).map(|n| n * 2);
        assert_eq!(page.items, vec![2, 4]);
        assert_eq!(
            (page.page, page.per_page, page.total),
            (Some(3), Some(2), Some(57))
        );

        let page = Page::from_offset(vec![1u32, 2], 3, 2, 57)
            .try_map(|n| Ok(n * 2))
            .expect("all convert");
        assert_eq!((page.page, page.per_page), (Some(3), Some(2)));
    }

    #[test]
    fn the_envelope_omits_what_is_absent() {
        let page = Page::new(vec![1u32]);
        assert_eq!(serde_json::to_value(&page).unwrap(), json!({"items": [1]}));

        let page = Page::new(vec![1u32])
            .with_next(cursor(b"foo"))
            .with_total(1);
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            json!({"items": [1], "next_cursor": "Zm9v", "total": 1})
        );
    }

    #[test]
    fn a_page_is_a_200_of_json() {
        let response = Page::new(vec![1u32]).into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn a_page_registers_a_named_component_per_row_type() {
        assert_eq!(Page::<u32>::schema_name(), "Page_UInt32");
        let mut generator = SchemaGenerator::default();
        let node = generator.subschema_for::<Page<u32>>();
        assert!(node.is_reference(), "the envelope is a component: {node:?}");

        let definition = generator
            .definitions()
            .get(Page::<u32>::schema_name().as_ref())
            .expect("registered");
        assert_eq!(definition.required, vec!["items"]);
        for member in [
            "items",
            "next_cursor",
            "prev_cursor",
            "total",
            "page",
            "per_page",
        ] {
            assert!(
                definition.properties.contains_key(member),
                "`{member}` is in the envelope but not in its schema"
            );
        }
    }

    #[test]
    fn a_page_documents_itself_at_200() {
        let op = described::<Page<u32>>();
        let response = op.response(200).expect("200 documented");
        let schema = response
            .content
            .get("application/json")
            .and_then(|m| m.schema.as_ref())
            .expect("a JSON schema");
        assert_eq!(
            schema.reference.as_deref(),
            Some("#/components/schemas/Page_UInt32")
        );
    }

    /// A row that is only valid when it is even, so a failure has somewhere to
    /// come from.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Even(u32);

    impl Validate for Even {
        fn validate(&self, ctx: &mut ValidationCtx) -> core::result::Result<(), ValidationErrors> {
            if self.0.is_multiple_of(2) {
                Ok(())
            } else {
                Err(ValidationErrors::one(
                    ctx.pointer().to_owned(),
                    "custom:even",
                    "must be even",
                ))
            }
        }
    }

    #[test]
    fn a_page_validates_its_rows_and_points_at_the_offender() {
        let good = Page::new(vec![Even(2), Even(4)]);
        assert!(good.validate(&mut ValidationCtx::new()).is_ok());
        assert!(
            Page::<Even>::empty()
                .validate(&mut ValidationCtx::new())
                .is_ok()
        );

        let bad = Page::new(vec![Even(2), Even(3)]);
        let errors = bad
            .validate(&mut ValidationCtx::new())
            .expect_err("the second row is odd");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors.as_slice()[0].pointer, "/items/1");
        assert_eq!(errors.as_slice()[0].code, "custom:even");
    }

    #[test]
    fn links_rewrite_only_the_cursor() {
        let page = Page::new(vec![1u32])
            .with_next(cursor(b"foo"))
            .with_prev(cursor(b"bar"));
        let uri: http::Uri = "/posts?status=open&cursor=stale&limit=10".parse().unwrap();
        let links = PageLinks::for_page(&page, &uri);

        assert_eq!(
            links.next.as_deref(),
            Some("/posts?status=open&limit=10&cursor=Zm9v")
        );
        assert_eq!(
            links.prev.as_deref(),
            Some("/posts?status=open&limit=10&cursor=YmFy")
        );
        assert_eq!(links.first.as_deref(), Some("/posts?status=open&limit=10"));
    }

    #[test]
    fn links_survive_a_query_less_uri_and_an_absolute_one() {
        let page = Page::new(vec![1u32]).with_next(cursor(b"foo"));

        let uri: http::Uri = "/posts".parse().unwrap();
        let links = PageLinks::for_page(&page, &uri);
        assert_eq!(links.next.as_deref(), Some("/posts?cursor=Zm9v"));
        assert_eq!(links.first.as_deref(), Some("/posts"));

        let uri: http::Uri = "https://api.example.com/posts?a=1".parse().unwrap();
        let links = PageLinks::for_page(&page, &uri);
        assert_eq!(
            links.next.as_deref(),
            Some("https://api.example.com/posts?a=1&cursor=Zm9v")
        );
    }

    #[test]
    fn the_link_header_is_rfc_8288() {
        let links = PageLinks {
            next: Some("/posts?cursor=b".into()),
            prev: None,
            first: Some("/posts".into()),
        };
        assert_eq!(
            links.to_header().unwrap().to_str().unwrap(),
            "</posts?cursor=b>; rel=\"next\", </posts>; rel=\"first\""
        );
        assert!(PageLinks::default().to_header().is_none());
        assert!(PageLinks::default().is_empty());
    }
}
