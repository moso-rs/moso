//! Pagination: keyset by default, offset when page numbers are genuinely
//! needed.
//!
//! A cursor is opaque, signed with the application secret, and carries the
//! ordering it was issued for — so a tampered cursor is refused rather than
//! producing a strange page, and a cursor from a differently sorted query is
//! refused with a message that says so.
//!
//! ```no_run
//! # use moso_core::response::{Page, cursor::CursorCodec};
//! # use moso_orm::{Db, Entity, Result, Select};
//! # use moso_schema::types::Cursor;
//! async fn list<E: Entity>(db: &Db, codec: &CursorCodec, at: Option<Cursor>) -> Result<Page<E>> {
//!     Select::<E>::new()
//!         .paginate(at, 25)
//!         .signed_with(codec.clone())
//!         .fetch(db)
//!         .await
//! }
//! ```
//!
//! # What "keyset" means here, exactly
//!
//! The query's `ORDER BY` is turned into a **key**: a list of plain columns,
//! each with an explicit direction and an explicit `NULLS` placement, ending in
//! the primary key so the order is total. A cursor is the values of that key in
//! the last row the client saw. Resuming means adding the lexicographic
//! "strictly after this tuple" predicate, which an index on the same columns
//! serves with a seek rather than a scan.
//!
//! Three details are easy to get wrong and are handled here:
//!
//! * **The tiebreaker.** `ORDER BY created_at DESC` is not a total order: two
//!   rows sharing a timestamp can swap between requests, so a page boundary
//!   between them drops or repeats a row. The primary key is appended
//!   automatically, in the direction of the last term.
//! * **`NULLS FIRST` / `NULLS LAST`.** PostgreSQL defaults to `NULLS LAST` for
//!   `ASC` and `NULLS FIRST` for `DESC`; SQLite sorts `NULL` first in both.
//!   A query that paginates over a nullable column and does not say which it
//!   wants therefore returns *different pages on the two backends*. Every term
//!   this module emits carries an explicit placement — PostgreSQL's, made
//!   explicit — and the keyset predicate is built to match it, including the
//!   case where the cursor's own key value is `NULL`.
//! * **Paging backwards.** Running the query in reverse means reversing the
//!   direction *and* the `NULLS` placement of every term
//!   ([`OrderTerm::reversed`] does both), then flipping the rows back. The
//!   cursor's ordering fingerprint is computed from the forward form, so a
//!   forward cursor and a backward one are interchangeable.
//!
//! # Cost
//!
//! One statement per page. [`Paginated::with_total`] adds a second, which is
//! why it is opt-in: over a large filtered table the count is the expensive
//! half of the request. [`OffsetPaginated`] always issues both, because a page
//! number without a page count is not usable.

use core::marker::PhantomData;

use moso_core::response::Page;
use moso_core::response::cursor::CursorCodec;
use moso_schema::types::Cursor;
use moso_sql::{ColumnRef, Expr, Nulls, Order, OrderTerm, Value, ValueKind};

use crate::cursor::PageCursor;
use crate::entity::{ColumnDef, Entity, Ready};
use crate::error::{CursorError, Error, Result};
use crate::executor::Executor;
use crate::predicate::Predicate;
use crate::row::Row;
use crate::select::Select;

/// A keyset-paginated query.
///
/// The primary key is appended to the ordering as a tiebreaker, so the order is
/// always total and a page boundary can never sit between two equal keys.
///
/// ```
/// # use moso_orm::{Entity, Paginated, Select};
/// fn first_page<E: Entity>(query: Select<E>) -> Paginated<E> {
///     query.paginate(None, 25)
/// }
/// ```
pub struct Paginated<E, J = ()> {
    select: Select<E, J>,
    cursor: Option<Cursor>,
    limit: u32,
    with_total: bool,
    direction: PageDirection,
    codec: Option<CursorCodec>,
    scope: Option<&'static str>,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity, J> Paginated<E, J> {
    /// A page of `limit` rows, starting after `cursor`.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn page<E: Entity>(query: Select<E>) -> Paginated<E> {
    ///     Paginated::new(query, None, 20)
    /// }
    /// ```
    #[must_use]
    pub const fn new(select: Select<E, J>, cursor: Option<Cursor>, limit: u32) -> Self {
        Self {
            select,
            cursor,
            limit,
            with_total: false,
            direction: PageDirection::Forward,
            codec: None,
            scope: None,
            entity: PhantomData,
        }
    }

    /// Also runs a `count(*)`, so the page carries a total.
    ///
    /// Opt-in because it costs a second statement over the whole result set,
    /// which on a large table is the expensive half of the request.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn counted<E: Entity>(query: Select<E>) -> Paginated<E> {
    ///     query.paginate(None, 20).with_total()
    /// }
    /// ```
    #[must_use]
    pub const fn with_total(mut self) -> Self {
        self.with_total = true;
        self
    }

    /// Pages backwards from the cursor.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn back<E: Entity>(query: Select<E>) -> Paginated<E> {
    ///     query.paginate(None, 20).backward()
    /// }
    /// ```
    #[must_use]
    pub const fn backward(mut self) -> Self {
        self.direction = PageDirection::Backward;
        self
    }

    /// Signs and verifies this listing's cursors with `codec`.
    ///
    /// **Required.** A cursor that goes into a `WHERE` clause and is not
    /// authenticated is a query parameter the client can edit, so a page
    /// without a codec refuses to mint or accept one
    /// ([`CursorError::NoSigningKey`]) rather than issuing something that only
    /// looks opaque.
    ///
    /// Register one [`CursorCodec`] at boot and inject it:
    /// `App::new(cfg).provide(CursorCodec::new(secret))`, then
    /// `Inject(codec): Inject<CursorCodec>` in the handler. Cloning is cheap —
    /// the key is behind an `Arc`.
    ///
    /// ```
    /// # use moso_core::response::cursor::CursorCodec;
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn signed<E: Entity>(query: Select<E>, codec: &CursorCodec) -> Paginated<E> {
    ///     query.paginate(None, 20).signed_with(codec.clone())
    /// }
    /// ```
    #[must_use]
    pub fn signed_with(mut self, codec: CursorCodec) -> Self {
        self.codec = Some(codec);
        self
    }

    /// As [`Paginated::signed_with`], deriving the codec from a secret.
    ///
    /// Convenient in a test or a one-off script. An application should build
    /// one [`CursorCodec`] at boot and clone it, rather than re-deriving the
    /// HMAC key per request.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn signed<E: Entity>(query: Select<E>) -> Paginated<E> {
    ///     query.paginate(None, 20).signed_with_secret("a-32-byte-or-longer-signing-secret")
    /// }
    /// ```
    #[must_use]
    pub fn signed_with_secret(self, secret: impl AsRef<[u8]>) -> Self {
        self.signed_with(CursorCodec::new(secret))
    }

    /// Renames the listing a cursor is bound to.
    ///
    /// The scope is mixed into the signature and never transmitted, so a cursor
    /// minted by one listing cannot be replayed against another. It defaults to
    /// the entity's name, which separates listings of different entities;
    /// override it when two endpoints over the *same* entity must not share
    /// cursors.
    ///
    /// ```
    /// # use moso_orm::{Entity, Paginated, Select};
    /// fn inbox<E: Entity>(query: Select<E>) -> Paginated<E> {
    ///     query.paginate(None, 20).with_scope("inbox")
    /// }
    /// ```
    #[must_use]
    pub const fn with_scope(mut self, scope: &'static str) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The listing a cursor is bound to.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn scope<E: Entity>(query: Select<E>) -> &'static str {
    ///     query.paginate(None, 20).scope()
    /// }
    /// ```
    #[must_use]
    pub fn scope(&self) -> &'static str {
        self.scope.unwrap_or(E::NAME)
    }

    /// The cursor this page resumes from, when there is one.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn resuming<E: Entity>(query: Select<E>) -> bool {
    ///     query.paginate(None, 20).cursor().is_some()
    /// }
    /// ```
    #[must_use]
    pub fn cursor(&self) -> Option<&Cursor> {
        self.cursor.as_ref()
    }

    /// Whether the page will carry a total.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn counted<E: Entity>(query: Select<E>) -> bool {
    ///     query.paginate(None, 20).with_total().wants_total()
    /// }
    /// ```
    #[must_use]
    pub const fn wants_total(&self) -> bool {
        self.with_total
    }

    /// Which way the page runs.
    ///
    /// ```
    /// # use moso_orm::{Entity, PageDirection, Select};
    /// fn direction<E: Entity>(query: Select<E>) -> PageDirection {
    ///     query.paginate(None, 20).direction()
    /// }
    /// ```
    #[must_use]
    pub const fn direction(&self) -> PageDirection {
        self.direction
    }

    /// How many rows a page holds.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn size<E: Entity>(query: Select<E>) -> u32 {
    ///     query.paginate(None, 20).limit()
    /// }
    /// ```
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    /// The query underneath.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn filters<E: Entity>(query: Select<E>) -> usize {
    ///     query.paginate(None, 20).query().filters().len()
    /// }
    /// ```
    #[must_use]
    pub const fn query(&self) -> &Select<E, J> {
        &self.select
    }

    /// The ordering the page actually runs with: every term normalised to an
    /// explicit `NULLS` placement, the primary key appended as a tiebreaker,
    /// and the whole thing reversed when the page runs backwards.
    ///
    /// # Errors
    ///
    /// [`Error::Build`] when a term is not a plain column of the selected
    /// entity's table, and [`CursorError::NoOrder`] when there is nothing to
    /// order by at all.
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Select};
    /// # use moso_sql::OrderTerm;
    /// fn terms<E: Entity>(query: Select<E>) -> Result<Vec<OrderTerm>> {
    ///     query.paginate(None, 20).ordering()
    /// }
    /// ```
    pub fn ordering(&self) -> Result<Vec<OrderTerm>> {
        let key = key_columns::<E>(self.select.order_terms())?;
        Ok(order_terms(&key, self.direction))
    }

    /// The fingerprint basis: the ordering in canonical **forward** form,
    /// whichever way this page runs.
    ///
    /// A cursor carries the fingerprint of this, so a cursor from a
    /// differently-sorted query is refused, and a forward cursor still opens a
    /// backward page of the same query.
    ///
    /// # Errors
    ///
    /// As [`Paginated::ordering`].
    ///
    /// ```
    /// # use moso_orm::{Entity, OrderingKey, Result, Select};
    /// fn key<E: Entity>(query: Select<E>) -> Result<OrderingKey> {
    ///     query.paginate(None, 20).ordering_key()
    /// }
    /// ```
    pub fn ordering_key(&self) -> Result<OrderingKey> {
        let key = key_columns::<E>(self.select.order_terms())?;
        Ok(ordering_key(&key))
    }

    /// The `WHERE` addition that resumes from the cursor, or `None` on the
    /// first page.
    ///
    /// ```text
    /// (a > $1) OR (a = $1 AND b < $2) OR (a = $1 AND b = $2 AND id > $3)
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Paginated::ordering`], plus [`CursorError::Tampered`] for a cursor
    /// this application did not mint, [`CursorError::OrderingChanged`] for one
    /// minted for a different sort, and [`CursorError::NoSigningKey`] when no
    /// [`CursorCodec`] was given.
    ///
    /// ```
    /// # use moso_orm::{Entity, Predicate, Result, Select};
    /// fn resume<E: Entity>(query: Select<E>) -> Result<Option<Predicate>> {
    ///     query.paginate(None, 20).keyset()
    /// }
    /// ```
    pub fn keyset(&self) -> Result<Option<Predicate>> {
        let Some(token) = &self.cursor else {
            return Ok(None);
        };
        let key = key_columns::<E>(self.select.order_terms())?;
        let codec = self.codec.as_ref().ok_or(CursorError::NoSigningKey)?;
        let opened = PageCursor::open(codec, self.scope(), token)?;
        if !opened.matches(ordering_key(&key).fingerprint(), key.len()) {
            return Err(CursorError::OrderingChanged.into());
        }
        Ok(Some(Predicate::of(
            [E::NAME],
            keyset_expr(&key, opened.key(), self.direction),
        )))
    }

    /// The query this page runs: the caller's, re-ordered, resumed from the
    /// cursor, and asking for one row more than the page holds.
    ///
    /// The extra row is how the page learns whether there is a next one without
    /// counting anything.
    ///
    /// # Errors
    ///
    /// As [`Paginated::keyset`].
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Select};
    /// fn planned<E: Entity>(query: Select<E>) -> Result<Select<E>> {
    ///     query.paginate(None, 20).to_select()
    /// }
    /// ```
    pub fn to_select(&self) -> Result<Select<E, J>> {
        let key = key_columns::<E>(self.select.order_terms())?;
        self.plan(&key)
    }

    /// [`Paginated::to_select`], with the key columns already resolved.
    fn plan(&self, key: &[KeyColumn]) -> Result<Select<E, J>> {
        let mut query = self.select.clone().clear_order();
        for term in order_terms(key, self.direction) {
            query = query.order_by(term);
        }
        if let Some(token) = &self.cursor {
            let codec = self.codec.as_ref().ok_or(CursorError::NoSigningKey)?;
            let opened = PageCursor::open(codec, self.scope(), token)?;
            if !opened.matches(ordering_key(key).fingerprint(), key.len()) {
                return Err(CursorError::OrderingChanged.into());
            }
            query = query.filter(Predicate::of(
                [E::NAME],
                keyset_expr(key, opened.key(), self.direction),
            ));
        }
        Ok(query.limit(u64::from(self.limit).saturating_add(1)))
    }

    /// Seals one row's sort key into a cursor.
    fn mint(&self, ordering: u64, key: &[Value]) -> Result<Cursor> {
        let codec = self.codec.as_ref().ok_or(CursorError::NoSigningKey)?;
        PageCursor::new(ordering, key.iter().cloned()).seal(codec, self.scope())
    }
}

impl<E: Entity, J: Ready<E>> Paginated<E, J> {
    /// Runs the page.
    ///
    /// One statement, plus one per `.with(..)` preload (non-negotiable N3),
    /// plus one more when [`Paginated::with_total`] was asked for.
    ///
    /// # Why the rows are decoded here and not by `Select::fetch_all`
    ///
    /// The sort key has to be read back out of the same rows the page returns,
    /// or the next cursor would cost a second statement. Since the entity's
    /// projection is `E::COLUMNS` in order, the key columns are already in the
    /// row and are read by index — so this costs nothing and the preloads still
    /// run, through the same [`run_preloads`](crate::relation::run_preloads)
    /// that `Select::fetch_all` uses.
    ///
    /// # Errors
    ///
    /// [`CursorError`] for a malformed, tampered or mismatched cursor, plus
    /// anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_core::response::Page;
    /// # use moso_orm::{Entity, Executor, Paginated, Result};
    /// async fn run<E: Entity>(page: Paginated<E>, ex: impl Executor<'_>) -> Result<Page<E>> {
    ///     page.fetch(ex).await
    /// }
    /// ```
    pub async fn fetch(self, executor: impl Executor<'_>) -> Result<Page<E>> {
        let handle = executor.handle();
        let key = key_columns::<E>(self.select.order_terms())?;
        let ordering = ordering_key(&key).fingerprint();

        let statement = self.plan(&key)?.to_statement()?;
        let rows = handle.fetch_all(&statement).await?;

        // One row more than the page holds was asked for; its presence is the
        // whole "is there another page" mechanism, and it never reaches the
        // client.
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let more = rows.len() > limit;
        let kept = rows.get(..limit).unwrap_or(&rows);

        let mut items = Vec::with_capacity(kept.len());
        let mut keys = Vec::with_capacity(kept.len());
        for row in kept {
            items.push(E::from_row(row)?);
            keys.push(read_key(row, &key)?);
        }
        if self.direction.reverses_the_order() {
            // The statement ran in reverse, so the rows arrived in reverse.
            items.reverse();
            keys.reverse();
        }

        // The same batched loader `Select::fetch_all` uses: one statement per
        // relation, whatever the page holds.
        if !self.select.preloads().is_empty() && !items.is_empty() {
            crate::relation::run_preloads(self.select.preloads(), &mut items, handle).await?;
        }

        let resuming = self.cursor.is_some();
        let (next, previous) = match self.direction {
            // Paging backwards: the extra row proves there is a *previous*
            // page, and the page we came from is the next one.
            PageDirection::Backward => (resuming, more),
            _ => (more, resuming),
        };

        let mut page = Page::new(items);
        if let Some(last) = keys.last().filter(|_| next) {
            page = page.with_next(self.mint(ordering, last)?);
        }
        if let Some(first) = keys.first().filter(|_| previous) {
            page = page.with_prev(self.mint(ordering, first)?);
        }
        if self.with_total {
            // Over the caller's filters, not the keyset predicate: a total that
            // shrank with every page would be a row count, not a total.
            let counted = self.select.clone().clear_order();
            let total = match handle.transaction() {
                Some(tx) => counted.count(tx).await?,
                None => counted.count(handle.db()).await?,
            };
            page = page.with_total(total);
        }
        Ok(page)
    }
}

impl<E: Entity, J> core::fmt::Debug for Paginated<E, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The codec is deliberately absent: it holds the application secret,
        // and `CursorCodec`'s own `Debug` redacts for the same reason.
        f.debug_struct("Paginated")
            .field("entity", &E::NAME)
            .field("limit", &self.limit)
            .field("direction", &self.direction)
            .field("with_total", &self.with_total)
            .field("signed", &self.codec.is_some())
            .finish_non_exhaustive()
    }
}

/// An offset-paginated query, for an admin screen that needs page numbers.
///
/// Offset pagination is O(offset) on every backend and drifts when rows are
/// inserted between requests. It is here because page numbers are sometimes a
/// requirement, and it is not the default because it is the wrong tool for a
/// public API.
///
/// ```
/// # use moso_orm::{Entity, OffsetPaginated, Select};
/// fn page_three<E: Entity>(query: Select<E>) -> OffsetPaginated<E> {
///     query.paginate_offset(3, 25)
/// }
/// ```
pub struct OffsetPaginated<E, J = ()> {
    select: Select<E, J>,
    page: u32,
    per_page: u32,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity, J> OffsetPaginated<E, J> {
    /// Page `page` (one-based) of `per_page` rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, OffsetPaginated, Select};
    /// fn page<E: Entity>(query: Select<E>) -> OffsetPaginated<E> {
    ///     OffsetPaginated::new(query, 1, 25)
    /// }
    /// ```
    #[must_use]
    pub const fn new(select: Select<E, J>, page: u32, per_page: u32) -> Self {
        Self {
            select,
            page,
            per_page,
            entity: PhantomData,
        }
    }

    /// The one-based page number.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn which<E: Entity>(query: Select<E>) -> u32 {
    ///     query.paginate_offset(2, 10).page()
    /// }
    /// ```
    #[must_use]
    pub const fn page(&self) -> u32 {
        self.page
    }

    /// How many rows a page holds.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn size<E: Entity>(query: Select<E>) -> u32 {
    ///     query.paginate_offset(2, 10).per_page()
    /// }
    /// ```
    #[must_use]
    pub const fn per_page(&self) -> u32 {
        self.per_page
    }

    /// The row offset this page starts at.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// # fn offset<E: Entity>(query: Select<E>) -> u64 {
    /// query.paginate_offset(3, 25).offset()
    /// # }
    /// ```
    #[must_use]
    pub const fn offset(&self) -> u64 {
        (self.page.saturating_sub(1) as u64).saturating_mul(self.per_page as u64)
    }

    /// The query underneath.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn filters<E: Entity>(query: Select<E>) -> usize {
    ///     query.paginate_offset(1, 10).query().filters().len()
    /// }
    /// ```
    #[must_use]
    pub const fn query(&self) -> &Select<E, J> {
        &self.select
    }

    /// The query this page runs: the caller's, with the primary key appended
    /// to the ordering and `LIMIT`/`OFFSET` applied.
    ///
    /// The tiebreaker matters even more here than for a cursor: page numbers
    /// over a non-total order are page numbers over an arbitrary permutation,
    /// and rows move between pages for no reason a user can see.
    ///
    /// ```
    /// # use moso_orm::{Entity, Select};
    /// fn planned<E: Entity>(query: Select<E>) -> Select<E> {
    ///     query.paginate_offset(2, 25).to_select()
    /// }
    /// ```
    #[must_use]
    pub fn to_select(&self) -> Select<E, J> {
        with_tiebreaker(self.select.clone())
            .limit(u64::from(self.per_page.max(1)))
            .offset(self.offset())
    }
}

impl<E: Entity, J: Ready<E>> OffsetPaginated<E, J> {
    /// Runs the page, always with a total — a page number without a page count
    /// is not usable.
    ///
    /// Two statements: the rows, and the count over the same filters.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_core::response::Page;
    /// # use moso_orm::{Entity, Executor, OffsetPaginated, Result};
    /// async fn run<E: Entity>(p: OffsetPaginated<E>, ex: impl Executor<'_>) -> Result<Page<E>> {
    ///     p.fetch(ex).await
    /// }
    /// ```
    pub async fn fetch(self, executor: impl Executor<'_>) -> Result<Page<E>> {
        let handle = executor.handle();
        let per_page = self.per_page.max(1);
        let number = self.page.max(1);

        let rows = self.to_select();
        let items = match handle.transaction() {
            Some(tx) => rows.fetch_all(tx).await?,
            None => rows.fetch_all(handle.db()).await?,
        };

        let counted = self.select.clear_order();
        let total = match handle.transaction() {
            Some(tx) => counted.count(tx).await?,
            None => counted.count(handle.db()).await?,
        };

        Ok(Page::from_offset(items, number, per_page, total))
    }
}

impl<E: Entity, J> core::fmt::Debug for OffsetPaginated<E, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OffsetPaginated")
            .field("entity", &E::NAME)
            .field("page", &self.page)
            .field("per_page", &self.per_page)
            .finish_non_exhaustive()
    }
}

/// Which way a keyset page runs.
///
/// ```
/// use moso_orm::PageDirection;
///
/// assert_eq!(PageDirection::default(), PageDirection::Forward);
/// assert!(PageDirection::Backward.reverses_the_order());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PageDirection {
    /// After the cursor, in the query's order.
    #[default]
    Forward,
    /// Before the cursor, which means running the query in reverse and
    /// flipping the rows back.
    Backward,
}

impl PageDirection {
    /// Whether the statement's `ORDER BY` is flipped.
    ///
    /// ```
    /// use moso_orm::PageDirection;
    ///
    /// assert!(!PageDirection::Forward.reverses_the_order());
    /// ```
    #[must_use]
    pub const fn reverses_the_order(self) -> bool {
        matches!(self, Self::Backward)
    }
}

/// The ordering a cursor was issued for.
///
/// A cursor carries this fingerprint, so resuming a differently sorted query is
/// refused with [`CursorError::OrderingChanged`] instead of silently skipping
/// or repeating rows.
///
/// ```
/// use moso_orm::OrderingKey;
///
/// let by_date = OrderingKey::of(["created_at", "id"]);
/// assert_eq!(by_date, OrderingKey::of(["created_at", "id"]));
/// assert_ne!(by_date, OrderingKey::of(["title", "id"]));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderingKey(Vec<String>);

impl OrderingKey {
    /// The fingerprint of an ordering, in key order.
    ///
    /// ```
    /// use moso_orm::OrderingKey;
    ///
    /// assert_eq!(OrderingKey::of(["id"]).columns(), ["id"]);
    /// ```
    #[must_use]
    pub fn of(columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(columns.into_iter().map(Into::into).collect())
    }

    /// The columns, in key order.
    ///
    /// ```
    /// use moso_orm::OrderingKey;
    ///
    /// assert_eq!(OrderingKey::of(["a", "b"]).columns().len(), 2);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.0
    }

    /// The 64-bit value a cursor actually carries.
    ///
    /// Spelling the whole ordering into every cursor would cost forty bytes of
    /// a token that has to fit in a URL, and would tell a client which columns
    /// it is being sorted by. See [`crate::cursor::fingerprint`] for why a
    /// non-cryptographic hash is the right tool here.
    ///
    /// ```
    /// use moso_orm::OrderingKey;
    ///
    /// let by_date = OrderingKey::of(["created_at desc", "id desc"]);
    /// assert_eq!(by_date.fingerprint(), OrderingKey::of(["created_at desc", "id desc"]).fingerprint());
    /// assert_ne!(by_date.fingerprint(), OrderingKey::of(["id asc"]).fingerprint());
    /// ```
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        crate::cursor::fingerprint(&self.0)
    }

    /// Whether a cursor issued for `other` may resume this query.
    ///
    /// # Errors
    ///
    /// [`CursorError::OrderingChanged`] when the orderings differ.
    ///
    /// ```
    /// use moso_orm::OrderingKey;
    ///
    /// let key = OrderingKey::of(["id"]);
    /// assert!(key.check(&OrderingKey::of(["id"])).is_ok());
    /// assert!(key.check(&OrderingKey::of(["name"])).is_err());
    /// ```
    pub fn check(&self, other: &Self) -> core::result::Result<(), CursorError> {
        if self == other {
            return Ok(());
        }
        Err(CursorError::OrderingChanged)
    }
}

/// One column of the keyset, resolved against the entity.
///
/// `index` is the column's position in `E::COLUMNS`, which is also its position
/// in the row, because the projection is built from the same constant list.
/// That is what lets the sort key be read back without adding anything to the
/// `SELECT` list.
#[derive(Clone, Debug, PartialEq)]
struct KeyColumn {
    column: ColumnRef,
    index: usize,
    kind: ValueKind,
    nullable: bool,
    order: Order,
    nulls: Nulls,
}

impl KeyColumn {
    /// The canonical spelling that goes into the ordering fingerprint.
    fn canonical(&self) -> String {
        format!(
            "{} {} nulls {}",
            self.column,
            match self.order {
                Order::Desc => "desc",
                _ => "asc",
            },
            match self.nulls {
                Nulls::First => "first",
                _ => "last",
            }
        )
    }
}

/// The `NULLS` placement PostgreSQL uses when a term does not say.
///
/// Made explicit on every emitted term, so SQLite — which sorts `NULL` first in
/// both directions — produces the same pages.
const fn default_nulls(order: Order) -> Nulls {
    match order {
        Order::Desc => Nulls::First,
        _ => Nulls::Last,
    }
}

/// `E`'s table-qualified reference to `column`.
fn qualified<E: Entity>(column: &ColumnDef) -> ColumnRef {
    ColumnRef::qualified(E::TABLE.name().clone(), column.ident())
}

/// Resolves the query's `ORDER BY` into a keyset, appending the primary key.
fn key_columns<E: Entity>(order: &[OrderTerm]) -> Result<Vec<KeyColumn>> {
    let mut key: Vec<KeyColumn> = Vec::with_capacity(order.len() + 1);

    for term in order {
        let Some(reference) = term.expr().as_column() else {
            return Err(not_a_column());
        };
        if let Some(qualifier) = reference.qualifier()
            && qualifier.as_str() != E::TABLE.name().as_str()
        {
            return Err(not_a_column());
        }
        let name = reference.name().as_str();
        let Some(index) = E::COLUMNS.iter().position(|column| column.name() == name) else {
            return Err(not_a_column());
        };
        let definition = &E::COLUMNS[index];
        let direction = term.order();
        key.push(KeyColumn {
            column: qualified::<E>(definition),
            index,
            kind: definition.kind(),
            nullable: definition.is_nullable(),
            order: direction,
            nulls: term.nulls().unwrap_or(default_nulls(direction)),
        });
    }

    // The tiebreaker, in the direction of the last term so one index can serve
    // the whole ordering.
    let direction = key.last().map_or(Order::Asc, |last| last.order);
    for (index, definition) in E::COLUMNS.iter().enumerate() {
        if !definition.is_primary_key() || key.iter().any(|column| column.index == index) {
            continue;
        }
        key.push(KeyColumn {
            column: qualified::<E>(definition),
            index,
            kind: definition.kind(),
            nullable: false,
            order: direction,
            nulls: default_nulls(direction),
        });
    }

    if key.is_empty() {
        return Err(CursorError::NoOrder.into());
    }
    Ok(key)
}

/// The `ORDER BY` the statement runs with.
fn order_terms(key: &[KeyColumn], direction: PageDirection) -> Vec<OrderTerm> {
    key.iter()
        .map(|column| {
            let term = OrderTerm::new(Expr::column(column.column.clone()), column.order)
                .with_nulls(Some(column.nulls));
            if direction.reverses_the_order() {
                // Flips the direction *and* the NULLS placement; reversing only
                // the direction moves NULLs to the other end of the result and
                // silently breaks the backward page.
                term.reversed()
            } else {
                term
            }
        })
        .collect()
}

/// The fingerprint basis, always in forward form.
fn ordering_key(key: &[KeyColumn]) -> OrderingKey {
    OrderingKey::of(key.iter().map(KeyColumn::canonical))
}

/// The lexicographic "strictly after this tuple" predicate.
///
/// ```text
/// (a > $1) OR (a = $1 AND b < $2) OR (a = $1 AND b = $2 AND id > $3)
/// ```
///
/// The row-value form `(a, b, id) > ($1, $2, $3)` is shorter and is what an
/// index likes best, but it is only correct when every term runs the same way
/// and no column is nullable. The expansion is correct for every combination of
/// direction and `NULLS` placement, so it is what is emitted; PostgreSQL still
/// uses the index for the leading term.
fn keyset_expr(key: &[KeyColumn], values: &[Value], direction: PageDirection) -> Expr {
    let mut disjuncts: Vec<Expr> = Vec::with_capacity(key.len());
    for (index, column) in key.iter().enumerate() {
        let Some(value) = values.get(index) else {
            break;
        };
        let Some(after) = strictly_after(column, value, direction) else {
            // Nothing sorts after a NULL that sorts last; the disjunct is
            // `FALSE` and is dropped rather than emitted.
            continue;
        };
        let mut conjuncts: Vec<Expr> = Vec::with_capacity(index + 1);
        for (earlier, earlier_value) in key.iter().zip(values).take(index) {
            conjuncts.push(equal_to(earlier, earlier_value));
        }
        conjuncts.push(after);
        if let Some(conjunct) = Expr::all_of(conjuncts) {
            disjuncts.push(conjunct.nested());
        }
    }
    // Every disjunct dropped means the cursor sits on the very last row in the
    // ordering, so the next page is empty. Saying so is cheaper than a scan.
    Expr::any_of(disjuncts).unwrap_or_else(|| Expr::value(false))
}

/// `column = value`, or `column IS NULL` for a `NULL` key value.
///
/// `=` rather than `IS NOT DISTINCT FROM` because the two agree whenever the
/// bound value is not `NULL`, and `=` is the one an index serves.
fn equal_to(column: &KeyColumn, value: &Value) -> Expr {
    let reference = Expr::column(column.column.clone());
    if value.is_null() {
        return reference.is_null();
    }
    reference.eq(Expr::bound(value.clone()))
}

/// The rows that sort strictly after `value` in this term, or `None` when
/// nothing does.
fn strictly_after(column: &KeyColumn, value: &Value, direction: PageDirection) -> Option<Expr> {
    let (order, nulls) = if direction.reverses_the_order() {
        (column.order.reversed(), column.nulls.reversed())
    } else {
        (column.order, column.nulls)
    };
    let reference = Expr::column(column.column.clone());

    if value.is_null() {
        return match nulls {
            // NULLs sort before every value, so everything non-NULL is after.
            Nulls::First => Some(reference.is_not_null()),
            // NULLs sort last, so nothing is after one.
            _ => None,
        };
    }

    let bound = Expr::bound(value.clone());
    let base = match order {
        Order::Desc => reference.clone().lt(bound),
        _ => reference.clone().gt(bound),
    };
    Some(match nulls {
        // NULLs sort after every value, so they are after this one too — but
        // only a nullable column has any, and the extra disjunct would cost an
        // index scan on a column that can never produce one.
        Nulls::Last if column.nullable => base.or(reference.is_null()).nested(),
        _ => base,
    })
}

/// Reads one row's sort key out of the columns the projection already selected.
fn read_key(row: &Row, key: &[KeyColumn]) -> Result<Vec<Value>> {
    let mut values = Vec::with_capacity(key.len());
    for column in key {
        values.push(read_key_value(row, column)?);
    }
    Ok(values)
}

/// Reads one sort-key column, as the [`Value`] that will be bound back.
fn read_key_value(row: &Row, column: &KeyColumn) -> Result<Value> {
    let index = column.index;
    if column.nullable && row.is_null(index)? {
        return Ok(Value::null(column.kind));
    }
    let value = match column.kind {
        ValueKind::Bool => Value::Bool(row.get_bool(index)?),
        ValueKind::I8 => Value::I8(narrow(i64::from(row.get_i16(index)?), index)?),
        ValueKind::I16 => Value::I16(row.get_i16(index)?),
        ValueKind::I32 => Value::I32(row.get_i32(index)?),
        ValueKind::I64 => Value::I64(row.get_i64(index)?),
        ValueKind::U8 => Value::U8(narrow(i64::from(row.get_i16(index)?), index)?),
        ValueKind::U16 => Value::U16(narrow(i64::from(row.get_i32(index)?), index)?),
        ValueKind::U32 => Value::U32(narrow(row.get_i64(index)?, index)?),
        ValueKind::U64 => Value::U64(narrow(row.get_i64(index)?, index)?),
        ValueKind::F32 => Value::F32(row.get_f32(index)?),
        ValueKind::F64 => Value::F64(row.get_f64(index)?),
        ValueKind::Decimal => Value::Decimal(row.get_decimal(index)?),
        ValueKind::Text => Value::text(row.get_string(index)?),
        ValueKind::Bytes => Value::bytes(row.get_bytes(index)?.to_vec()),
        ValueKind::Uuid => Value::Uuid(moso_sql::Uuid::from_bytes(
            row.get_uuid(index)?.into_bytes(),
        )),
        ValueKind::Json => Value::json(&row.get_json_text(index)?).map_err(build)?,
        ValueKind::Timestamp => {
            let at = row.get_timestamp(index)?;
            Value::Timestamp(
                moso_sql::Timestamp::new(at.timestamp(), at.timestamp_subsec_nanos())
                    .map_err(build)?,
            )
        }
        ValueKind::DateTime => {
            let at = row.get_datetime(index)?;
            Value::DateTime(moso_sql::DateTime::new(
                date_of(at.date())?,
                time_of(at.time())?,
            ))
        }
        ValueKind::Date => Value::Date(date_of(row.get_date(index)?)?),
        ValueKind::Time => Value::Time(time_of(row.get_time(index)?)?),
        // `Interval`, `Array`, `Unknown`, and anything a later `moso-sql` adds.
        _ => return Err(not_a_sort_key()),
    };
    Ok(value)
}

/// Narrows a widened integer back to the type the column declares.
fn narrow<T: TryFrom<i64>>(value: i64, index: usize) -> Result<T> {
    T::try_from(value).map_err(|_| {
        Error::Decode(crate::row::DecodeError::malformed(
            index,
            core::any::type_name::<T>(),
            format!("the server returned {value}, which does not fit"),
        ))
    })
}

/// A `chrono` date as `moso-sql`'s.
fn date_of(date: chrono::NaiveDate) -> Result<moso_sql::Date> {
    use chrono::Datelike as _;

    let month = u8::try_from(date.month()).map_err(|_| not_a_sort_key())?;
    let day = u8::try_from(date.day()).map_err(|_| not_a_sort_key())?;
    moso_sql::Date::new(date.year(), month, day).map_err(build)
}

/// A `chrono` time as `moso-sql`'s.
fn time_of(time: chrono::NaiveTime) -> Result<moso_sql::Time> {
    use chrono::Timelike as _;

    let hour = u8::try_from(time.hour()).map_err(|_| not_a_sort_key())?;
    let minute = u8::try_from(time.minute()).map_err(|_| not_a_sort_key())?;
    let second = u8::try_from(time.second()).map_err(|_| not_a_sort_key())?;
    moso_sql::Time::new(hour, minute, second, time.nanosecond()).map_err(build)
}

/// A `moso-sql` value error, as this crate's error.
fn build(error: moso_sql::ValueError) -> Error {
    Error::Build(error.into())
}

/// The `ORDER BY` a keyset cannot read back.
fn not_a_column() -> Error {
    Error::Build(moso_sql::Error::InvalidClause {
        clause: "ORDER BY",
        reason: "keyset pagination reads the sort key back out of each row, so every term must \
                 be a plain column of the entity being selected",
        help: "order by the entity's own columns — a joined column belongs in a `filter`, and a \
               computed one in a projection — or use `paginate_offset(page, per_page)`",
    })
}

/// The `ORDER BY` column whose type is not a sort key.
fn not_a_sort_key() -> Error {
    Error::Build(moso_sql::Error::InvalidClause {
        clause: "ORDER BY",
        reason: "this column's type cannot be carried in a pagination cursor; an interval or an \
                 array is not a sort key",
        help: "order by an id, a timestamp, a number or a text column, or use \
               `paginate_offset(page, per_page)`",
    })
}

/// Appends the primary key to an ordering that does not already contain it.
///
/// The lax counterpart of [`key_columns`]: offset pagination never reads the
/// sort key back, so an expression term is fine and only the tiebreaker
/// matters.
fn with_tiebreaker<E: Entity, J>(query: Select<E, J>) -> Select<E, J> {
    let direction = query
        .order_terms()
        .last()
        .map_or(Order::Asc, OrderTerm::order);
    let already: Vec<String> = query
        .order_terms()
        .iter()
        .filter_map(|term| term.expr().as_column())
        .map(|column| column.name().as_str().to_owned())
        .collect();

    let mut ordered = query;
    for definition in E::COLUMNS.iter().filter(|column| column.is_primary_key()) {
        if already.iter().any(|name| name == definition.name()) {
            continue;
        }
        ordered = ordered.order_by(
            OrderTerm::new(Expr::column(qualified::<E>(definition)), direction)
                .with_nulls(Some(default_nulls(direction))),
        );
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::Column;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::DecodeError;
    use moso_sql::TableRef;
    use std::sync::OnceLock;

    /// A post, with a nullable sort column and a primary key.
    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("posts");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("title", ValueKind::Text),
            ColumnDef::new("published_at", ValueKind::Timestamp).nullable(),
        ];
        const NAME: &'static str = "Post";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
        }
    }

    /// A row with no primary key at all, to exercise the refusal.
    #[derive(Clone, Debug)]
    struct Ledger;

    impl Entity for Ledger {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("ledger");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("amount", ValueKind::I64)];
        const NAME: &'static str = "Ledger";

        fn pk(&self) -> i64 {
            0
        }

        fn from_row(_row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self)
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Ledger", Self::TABLE).build())
        }
    }

    const ID: Column<Post, i64> = Column::new("id");
    const TITLE: Column<Post, String> = Column::new("title");
    const PUBLISHED_AT: Column<Post, Option<chrono::DateTime<chrono::Utc>>> =
        Column::new("published_at");

    fn secret() -> &'static str {
        "the application secret, which is long enough"
    }

    /// The top-level `OR` branches of a keyset predicate, in order.
    ///
    /// `Expr::any_of` folds left, so the tree is `((a OR b) OR c)`.
    fn disjuncts(expr: &Expr) -> Vec<&Expr> {
        match expr {
            Expr::Binary {
                lhs,
                op: moso_sql::BinOp::Or,
                rhs,
            } => {
                let mut branches = disjuncts(lhs);
                branches.extend(disjuncts(rhs));
                branches
            }
            other => vec![other],
        }
    }

    /// Every binary operator in the tree, in traversal order.
    fn operators(expr: &Expr) -> Vec<moso_sql::BinOp> {
        let mut found = Vec::new();
        walk(expr, &mut |node| {
            if let Expr::Binary { op, .. } = node {
                found.push(*op);
            }
        });
        found
    }

    /// The `negated` flag of every `IS [NOT] NULL` in the tree.
    fn null_tests(expr: &Expr) -> Vec<bool> {
        let mut found = Vec::new();
        walk(expr, &mut |node| {
            if let Expr::IsNull { negated, .. } = node {
                found.push(*negated);
            }
        });
        found
    }

    /// Visits every node of an expression this module can build.
    fn walk(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
        visit(expr);
        match expr {
            Expr::Nested(inner) => walk(inner, visit),
            Expr::Binary { lhs, rhs, .. } => {
                walk(lhs, visit);
                walk(rhs, visit);
            }
            Expr::IsNull { operand, .. } => walk(operand, visit),
            _ => {}
        }
    }

    #[test]
    fn the_offset_is_zero_based_from_a_one_based_page() {
        let first = Select::<Post>::new().paginate_offset(1, 25);
        assert_eq!(first.offset(), 0);

        let third = Select::<Post>::new().paginate_offset(3, 25);
        assert_eq!(third.offset(), 50);

        // Page zero is not a page; clamping is better than underflowing.
        let zero = Select::<Post>::new().paginate_offset(0, 25);
        assert_eq!(zero.offset(), 0);
    }

    #[test]
    fn an_ordering_key_refuses_a_cursor_from_another_sort() {
        let by_date = OrderingKey::of(["created_at", "id"]);
        assert!(
            by_date
                .check(&OrderingKey::of(["created_at", "id"]))
                .is_ok()
        );

        let error = by_date
            .check(&OrderingKey::of(["title", "id"]))
            .expect_err("a different sort");
        assert_eq!(error, CursorError::OrderingChanged);
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn every_cursor_error_offers_a_fix() {
        for error in [
            CursorError::Malformed,
            CursorError::Tampered,
            CursorError::OrderingChanged,
            CursorError::NoOrder,
            CursorError::NoSigningKey,
        ] {
            assert!(error.to_string().contains("help:"), "{error}");
        }
    }

    #[test]
    fn a_backward_page_reverses_the_order() {
        assert!(!PageDirection::Forward.reverses_the_order());
        assert!(PageDirection::Backward.reverses_the_order());
    }

    #[test]
    fn the_primary_key_is_appended_as_a_tiebreaker() {
        let key = key_columns::<Post>(&[TITLE.asc()]).expect("a keyset");
        assert_eq!(key.len(), 2, "title, then the tiebreaker");
        assert_eq!(key[0].column.name().as_str(), "title");
        assert_eq!(key[1].column.name().as_str(), "id");
        // The tiebreaker follows the last term's direction, so one index serves
        // the whole ordering.
        assert_eq!(key[1].order, Order::Asc);

        let descending = key_columns::<Post>(&[TITLE.desc()]).expect("a keyset");
        assert_eq!(descending[1].order, Order::Desc);
    }

    #[test]
    fn an_ordering_that_already_ends_in_the_key_is_left_alone() {
        let key = key_columns::<Post>(&[TITLE.asc(), ID.desc()]).expect("a keyset");
        assert_eq!(key.len(), 2);
        assert_eq!(key[1].column.name().as_str(), "id");
        assert_eq!(key[1].order, Order::Desc, "the user's direction survives");
    }

    #[test]
    fn an_unordered_query_paginates_by_the_primary_key_alone() {
        let key = key_columns::<Post>(&[]).expect("a keyset");
        assert_eq!(key.len(), 1);
        assert_eq!(key[0].column.name().as_str(), "id");
    }

    #[test]
    fn an_entity_with_no_key_and_no_order_is_refused_with_a_fix() {
        let error = key_columns::<Ledger>(&[]).expect_err("nothing to order by");
        assert!(matches!(error, Error::Cursor(CursorError::NoOrder)));
        assert!(error.to_string().contains("help:"));
    }

    #[test]
    fn every_emitted_term_carries_an_explicit_nulls_placement() {
        // The backend divergence this closes: PostgreSQL defaults `ASC` to
        // NULLS LAST and `DESC` to NULLS FIRST, SQLite sorts NULL first in
        // both, so a page over a nullable column would differ per backend.
        let key = key_columns::<Post>(&[PUBLISHED_AT.desc()]).expect("a keyset");
        assert_eq!(key[0].nulls, Nulls::First, "PostgreSQL's DESC default");

        let ascending = key_columns::<Post>(&[PUBLISHED_AT.asc()]).expect("a keyset");
        assert_eq!(ascending[0].nulls, Nulls::Last, "PostgreSQL's ASC default");

        for term in order_terms(&key, PageDirection::Forward) {
            assert!(term.nulls().is_some(), "{term:?}");
        }
    }

    #[test]
    fn an_explicit_nulls_placement_is_kept() {
        let key = key_columns::<Post>(&[PUBLISHED_AT.desc().nulls_last()]).expect("a keyset");
        assert_eq!(key[0].nulls, Nulls::Last);
    }

    #[test]
    fn paging_backwards_flips_both_the_direction_and_the_nulls() {
        let key = key_columns::<Post>(&[PUBLISHED_AT.asc()]).expect("a keyset");
        let forward = order_terms(&key, PageDirection::Forward);
        let backward = order_terms(&key, PageDirection::Backward);

        assert_eq!(forward[0].order(), Order::Asc);
        assert_eq!(forward[0].nulls(), Some(Nulls::Last));
        assert_eq!(backward[0].order(), Order::Desc);
        assert_eq!(
            backward[0].nulls(),
            Some(Nulls::First),
            "reversing only the direction would move NULLs to the other end"
        );
    }

    #[test]
    fn an_ordering_term_that_is_not_a_column_is_refused_with_a_fix() {
        let computed = OrderTerm::asc(ID.count());
        let error = key_columns::<Post>(&[computed]).expect_err("not a plain column");
        let text = error.to_string();
        assert!(text.contains("help:"), "{text}");
        assert!(text.contains("paginate_offset"), "{text}");
        assert!(error.is_programmer_error());
    }

    #[test]
    fn an_ordering_term_from_another_table_is_refused() {
        let elsewhere = OrderTerm::asc(Expr::column(ColumnRef::qualified(
            moso_sql::Ident::from_static("users"),
            moso_sql::Ident::from_static("name"),
        )));
        assert!(key_columns::<Post>(&[elsewhere]).is_err());

        let unknown = OrderTerm::asc(Expr::column(ColumnRef::from_static("nope")));
        assert!(key_columns::<Post>(&[unknown]).is_err());
    }

    #[test]
    fn the_fingerprint_is_direction_sensitive_and_table_qualified() {
        let ascending = ordering_key(&key_columns::<Post>(&[TITLE.asc()]).expect("a keyset"));
        let descending = ordering_key(&key_columns::<Post>(&[TITLE.desc()]).expect("a keyset"));
        assert_ne!(ascending.fingerprint(), descending.fingerprint());
        assert_eq!(ascending.columns()[0], "posts.title asc nulls last");
        assert_eq!(ascending.columns()[1], "posts.id asc nulls last");
    }

    #[test]
    fn the_fingerprint_does_not_depend_on_the_page_direction() {
        // A forward cursor must open a backward page of the same query.
        let forward = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 10);
        let backward = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 10)
            .backward();
        assert_eq!(
            forward.ordering_key().expect("a key").fingerprint(),
            backward.ordering_key().expect("a key").fingerprint()
        );
    }

    #[test]
    fn a_single_column_keyset_is_one_comparison() {
        let key = key_columns::<Post>(&[ID.asc()]).expect("a keyset");
        let expr = keyset_expr(&key, &[Value::I64(7)], PageDirection::Forward);
        assert_eq!(disjuncts(&expr).len(), 1);
        assert_eq!(operators(&expr), [moso_sql::BinOp::Gt]);
    }

    #[test]
    fn a_descending_keyset_compares_the_other_way() {
        let key = key_columns::<Post>(&[ID.desc()]).expect("a keyset");
        let expr = keyset_expr(&key, &[Value::I64(7)], PageDirection::Forward);
        assert_eq!(operators(&expr), [moso_sql::BinOp::Lt]);
    }

    #[test]
    fn a_backward_page_compares_the_other_way_again() {
        let key = key_columns::<Post>(&[ID.asc()]).expect("a keyset");
        let expr = keyset_expr(&key, &[Value::I64(7)], PageDirection::Backward);
        assert_eq!(
            operators(&expr),
            [moso_sql::BinOp::Lt],
            "backwards through an ascending sort"
        );
    }

    #[test]
    fn a_multi_column_keyset_expands_lexicographically() {
        let key = key_columns::<Post>(&[TITLE.asc()]).expect("title, then id");
        let expr = keyset_expr(
            &key,
            &[Value::text("hello"), Value::I64(7)],
            PageDirection::Forward,
        );
        // Two disjuncts: `title > 'hello'`, and `title = 'hello' AND id > 7`.
        let branches = disjuncts(&expr);
        assert_eq!(branches.len(), 2);
        assert_eq!(operators(branches[0]), [moso_sql::BinOp::Gt]);
        assert_eq!(
            operators(branches[1]),
            [
                moso_sql::BinOp::And,
                moso_sql::BinOp::Eq,
                moso_sql::BinOp::Gt
            ],
            "the leading term is pinned by equality, the next one advances"
        );
    }

    #[test]
    fn a_nullable_sort_column_that_sorts_last_admits_the_nulls() {
        // `published_at ASC NULLS LAST`: rows after a non-NULL value include
        // the NULL ones.
        let key = key_columns::<Post>(&[PUBLISHED_AT.asc()]).expect("a keyset");
        let value = Value::Timestamp(moso_sql::Timestamp::new(1, 0).expect("a timestamp"));
        let expr = keyset_expr(&key, &[value, Value::I64(7)], PageDirection::Forward);
        let branches = disjuncts(&expr);
        assert_eq!(branches.len(), 2);
        assert_eq!(
            null_tests(branches[0]),
            [false],
            "`published_at > $1 OR published_at IS NULL`"
        );
        assert!(operators(branches[0]).contains(&moso_sql::BinOp::Or));
    }

    #[test]
    fn a_nullable_sort_column_that_sorts_first_does_not_admit_the_nulls() {
        // `published_at DESC NULLS FIRST`: the NULLs are already behind us.
        let key = key_columns::<Post>(&[PUBLISHED_AT.desc()]).expect("a keyset");
        let value = Value::Timestamp(moso_sql::Timestamp::new(1, 0).expect("a timestamp"));
        let expr = keyset_expr(&key, &[value, Value::I64(7)], PageDirection::Forward);
        let branches = disjuncts(&expr);
        assert!(null_tests(branches[0]).is_empty());
        assert_eq!(operators(branches[0]), [moso_sql::BinOp::Lt]);
    }

    #[test]
    fn a_null_key_value_that_sorts_first_admits_everything_non_null() {
        // `published_at DESC NULLS FIRST` resumed from a NULL row: what comes
        // after is every non-NULL row.
        let key = key_columns::<Post>(&[PUBLISHED_AT.desc()]).expect("a keyset");
        let expr = keyset_expr(
            &key,
            &[Value::null(ValueKind::Timestamp), Value::I64(7)],
            PageDirection::Forward,
        );
        let branches = disjuncts(&expr);
        assert_eq!(branches.len(), 2);
        assert_eq!(null_tests(branches[0]), [true], "`IS NOT NULL`");
        // And the tiebreaker branch pins the NULL with `IS NULL`, not `= NULL`.
        assert_eq!(null_tests(branches[1]), [false]);
    }

    #[test]
    fn a_null_key_value_that_sorts_last_drops_its_disjunct() {
        // `published_at ASC NULLS LAST` resumed from a NULL row: nothing sorts
        // after it on that term, so only the tiebreaker can move the cursor.
        let key = key_columns::<Post>(&[PUBLISHED_AT.asc()]).expect("a keyset");
        let expr = keyset_expr(
            &key,
            &[Value::null(ValueKind::Timestamp), Value::I64(7)],
            PageDirection::Forward,
        );
        let branches = disjuncts(&expr);
        assert_eq!(branches.len(), 1, "the leading term's disjunct is dropped");
        assert_eq!(
            null_tests(branches[0]),
            [false],
            "the leading term is pinned with `IS NULL`"
        );
        assert!(operators(branches[0]).contains(&moso_sql::BinOp::Gt));
    }

    #[test]
    fn a_cursor_on_the_last_row_of_a_single_null_ordering_is_an_empty_page() {
        // Contrived, and the only way `keyset_expr` can produce no disjunct at
        // all: a one-column key whose value is a NULL that sorts last.
        let key = vec![KeyColumn {
            column: ColumnRef::from_static("published_at"),
            index: 2,
            kind: ValueKind::Timestamp,
            nullable: true,
            order: Order::Asc,
            nulls: Nulls::Last,
        }];
        let expr = keyset_expr(
            &key,
            &[Value::null(ValueKind::Timestamp)],
            PageDirection::Forward,
        );
        assert_eq!(expr, Expr::value(false));
    }

    #[test]
    fn the_planned_query_asks_for_one_row_more_than_the_page_holds() {
        let planned = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 25)
            .to_select()
            .expect("a plan");
        assert_eq!(planned.limit_value(), Some(26));
        assert_eq!(planned.order_terms().len(), 2, "title and the tiebreaker");
        assert!(planned.filters().is_empty(), "no cursor, no keyset filter");
    }

    #[test]
    fn a_cursor_adds_exactly_one_filter_and_no_extra_statement() {
        let query = Select::<Post>::new().order_by(TITLE.asc());
        let page = query
            .clone()
            .paginate(None, 10)
            .signed_with_secret(secret());

        let ordering = page.ordering_key().expect("a key").fingerprint();
        let token = PageCursor::new(ordering, [Value::text("hello"), Value::I64(7)])
            .seal(&CursorCodec::new(secret()), <Post as Entity>::NAME)
            .expect("signable");

        let resumed = query
            .paginate(Some(token), 10)
            .signed_with_secret(secret())
            .to_select()
            .expect("a plan");
        assert_eq!(resumed.filters().len(), 1);
        assert_eq!(resumed.filters()[0].predicate().entities(), ["Post"]);
    }

    #[test]
    fn a_cursor_from_a_differently_ordered_query_is_refused() {
        let by_title = Select::<Post>::new().order_by(TITLE.asc());
        let signed = by_title
            .clone()
            .paginate(None, 10)
            .signed_with_secret(secret());
        let token = PageCursor::new(
            signed.ordering_key().expect("a key").fingerprint(),
            [Value::text("hello"), Value::I64(7)],
        )
        .seal(&CursorCodec::new(secret()), "Post")
        .expect("signable");

        // Same listing, same secret, different sort.
        let by_id = Select::<Post>::new().order_by(ID.desc());
        let error = by_id
            .paginate(Some(token), 10)
            .signed_with_secret(secret())
            .to_select()
            .expect_err("a cursor from another sort");
        assert!(
            matches!(error, Error::Cursor(CursorError::OrderingChanged)),
            "{error}"
        );
        assert!(error.to_string().contains("restart from page one"));
    }

    #[test]
    fn a_tampered_cursor_is_refused_before_any_sql_is_built() {
        let page = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 10)
            .signed_with_secret(secret());
        let token = PageCursor::new(
            page.ordering_key().expect("a key").fingerprint(),
            [Value::text("hello"), Value::I64(7)],
        )
        .seal(&CursorCodec::new(secret()), "Post")
        .expect("signable");

        let mut bytes = token.into_bytes();
        bytes[2] ^= 0x80;

        let error = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(Some(Cursor::from_bytes(bytes)), 10)
            .signed_with_secret(secret())
            .to_select()
            .expect_err("edited");
        assert!(
            matches!(error, Error::Cursor(CursorError::Tampered)),
            "{error}"
        );
    }

    #[test]
    fn a_cursor_from_another_listing_of_the_same_entity_is_refused() {
        let page = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 10)
            .signed_with_secret(secret())
            .with_scope("inbox");
        assert_eq!(page.scope(), "inbox");

        let token = PageCursor::new(
            page.ordering_key().expect("a key").fingerprint(),
            [Value::text("hello"), Value::I64(7)],
        )
        .seal(&CursorCodec::new(secret()), "inbox")
        .expect("signable");

        // The default scope is the entity's name, which "inbox" is not.
        let error = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(Some(token), 10)
            .signed_with_secret(secret())
            .to_select()
            .expect_err("another listing's cursor");
        assert!(
            matches!(error, Error::Cursor(CursorError::Tampered)),
            "{error}"
        );
    }

    #[test]
    fn a_cursor_without_a_signing_key_is_refused_rather_than_trusted() {
        let token = PageCursor::new(0, [Value::I64(1)])
            .seal(&CursorCodec::new(secret()), "Post")
            .expect("signable");
        let error = Select::<Post>::new()
            .paginate(Some(token), 10)
            .to_select()
            .expect_err("unsigned");
        assert!(
            matches!(error, Error::Cursor(CursorError::NoSigningKey)),
            "{error}"
        );
        assert!(error.to_string().contains("app.secret_key"));
    }

    #[test]
    fn the_keyset_accessor_agrees_with_the_plan() {
        let page = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(None, 10)
            .signed_with_secret(secret());
        assert!(page.keyset().expect("no cursor").is_none());

        let token = PageCursor::new(
            page.ordering_key().expect("a key").fingerprint(),
            [Value::text("a"), Value::I64(1)],
        )
        .seal(&CursorCodec::new(secret()), "Post")
        .expect("signable");

        let resumed = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate(Some(token), 10)
            .signed_with_secret(secret());
        let predicate = resumed.keyset().expect("a keyset").expect("a cursor");
        assert_eq!(predicate.entities(), ["Post"]);
        let planned = resumed.to_select().expect("a plan");
        assert_eq!(planned.filters()[0].predicate().expr(), predicate.expr());
    }

    #[test]
    fn the_ordering_accessor_reports_what_the_statement_runs() {
        let terms = Select::<Post>::new()
            .order_by(TITLE.desc())
            .paginate(None, 10)
            .backward()
            .ordering()
            .expect("an ordering");
        assert_eq!(terms.len(), 2);
        assert_eq!(terms[0].order(), Order::Asc, "reversed for a backward page");
    }

    #[test]
    fn an_offset_page_is_deterministic_and_bounded() {
        let planned = Select::<Post>::new()
            .order_by(TITLE.asc())
            .paginate_offset(3, 25)
            .to_select();
        assert_eq!(planned.limit_value(), Some(25));
        assert_eq!(planned.offset_value(), Some(50));
        assert_eq!(
            planned.order_terms().len(),
            2,
            "the primary key is appended, or page numbers mean nothing"
        );
    }

    #[test]
    fn an_offset_page_does_not_duplicate_a_key_the_caller_already_ordered_by() {
        let planned = Select::<Post>::new()
            .order_by(ID.desc())
            .paginate_offset(1, 10)
            .to_select();
        assert_eq!(planned.order_terms().len(), 1);
        assert_eq!(planned.order_terms()[0].order(), Order::Desc);
    }

    #[test]
    fn an_offset_page_tolerates_a_zero_page_and_a_zero_size() {
        let planned = Select::<Post>::new().paginate_offset(0, 0).to_select();
        assert_eq!(planned.limit_value(), Some(1));
        assert_eq!(planned.offset_value(), Some(0));
    }

    #[test]
    fn an_offset_page_over_an_expression_ordering_is_allowed() {
        // Offset pagination never reads the sort key back, so a computed term
        // is fine — the restriction is a keyset one only.
        let planned = Select::<Post>::new()
            .order_by(OrderTerm::asc(ID.count()))
            .paginate_offset(1, 10)
            .to_select();
        assert_eq!(planned.order_terms().len(), 2);
    }

    #[test]
    fn a_page_never_prints_the_signing_key() {
        let printed = format!(
            "{:?}",
            Select::<Post>::new()
                .paginate(None, 10)
                .signed_with_secret(secret())
        );
        assert!(printed.contains("signed: true"), "{printed}");
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn the_accessors_report_what_was_asked_for() {
        let page = Select::<Post>::new().paginate(None, 33).with_total();
        assert_eq!(page.limit(), 33);
        assert!(page.wants_total());
        assert!(page.cursor().is_none());
        assert_eq!(page.scope(), "Post");
        assert_eq!(page.direction(), PageDirection::Forward);
        assert!(page.query().filters().is_empty());
    }
}

/// Keyset pagination against a real server.
///
/// PostgreSQL runs when `DATABASE_URL` is set and is skipped with a message
/// otherwise; SQLite always runs, against a temporary file.
///
/// The rows come back through the driver rather than through [`Row`]. That is
/// on purpose: what is proved here is the part this module owns — that the
/// *generated statement* pages correctly, seeing every row exactly once, in the
/// query's order, across a tie and across a `NULL`, and that a cursor minted
/// from one page opens the next. `Row`'s own decoders are covered by
/// `relation::preload::real_database`, which reads whole entities back.
#[cfg(all(test, feature = "postgres", feature = "sqlite"))]
mod real_database {
    use super::*;
    use crate::column::Column;
    use crate::db::Db;
    use crate::descriptor::EntityDescriptor;
    use crate::executor::Executor;
    use crate::row::DecodeError;
    use moso_sql::{Ident, Insert, RawStatement, Sql, TableRef};
    use std::sync::OnceLock;

    /// A scored item: an identifier, and a nullable score with plenty of ties.
    #[derive(Clone, Debug)]
    struct Item {
        id: i64,
    }

    impl Entity for Item {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("kp_items");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("score", ValueKind::I64).nullable(),
        ];
        const NAME: &'static str = "Item";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Item", Self::TABLE).build())
        }
    }

    const SCORE: Column<Item, Option<i64>> = Column::new("score");

    /// How many rows the fixture holds.
    const ROWS: i64 = 25;

    /// The signing secret every cursor in this suite is minted with.
    const SECRET: &str = "the pagination suite's application secret";

    /// The fixture, as `(id, score)`: one score in five is `NULL`, and the rest
    /// take four values, so every page boundary lands on a tie sooner or later.
    fn fixture_rows() -> Vec<(i64, Option<i64>)> {
        (1..=ROWS)
            .map(|id| (id, if id % 5 == 0 { None } else { Some(id % 4) }))
            .collect()
    }

    /// The ids in `score ASC NULLS LAST, id ASC` order.
    fn ascending() -> Vec<i64> {
        let mut rows = fixture_rows();
        rows.sort_by_key(|(id, score)| (u8::from(score.is_none()), score.unwrap_or(0), *id));
        rows.into_iter().map(|(id, _)| id).collect()
    }

    /// The ids in `score DESC NULLS FIRST, id DESC` order.
    fn descending() -> Vec<i64> {
        let mut rows = fixture_rows();
        rows.sort_by_key(|(id, score)| (u8::from(score.is_some()), -score.unwrap_or(0), -*id));
        rows.into_iter().map(|(id, _)| id).collect()
    }

    /// Drops and recreates the table, then fills it.
    async fn fixture(db: &Db) -> Result<()> {
        let handle = Executor::handle(db);
        for ddl in [
            "DROP TABLE IF EXISTS kp_items",
            "CREATE TABLE kp_items (id bigint PRIMARY KEY, score bigint)",
        ] {
            handle
                .execute(&RawStatement::new(ddl.to_owned()).into_statement())
                .await?;
        }
        let rows = fixture_rows().into_iter().map(|(id, score)| {
            vec![
                Expr::value(id),
                Expr::bound(score.map_or(Value::null(ValueKind::I64), Value::I64)),
            ]
        });
        handle
            .execute(
                &Insert::into_table(Item::TABLE)
                    .columns([Ident::from_static("id"), Ident::from_static("score")])
                    .rows(rows)
                    .into_statement(),
            )
            .await?;
        Ok(())
    }

    /// Runs a rendered statement through the driver, returning `(id, score)`.
    async fn run(db: &Db, sql: &Sql) -> Vec<(i64, Option<i64>)> {
        use sqlx::Row as _;

        // `AssertSqlSafe` because sqlx 0.9 only takes a `&'static str`
        // otherwise: the text here is one Moso rendered from a typed builder,
        // and every value in it is a bound parameter, which is exactly the
        // property the wrapper asserts.
        macro_rules! fetch {
            ($pool:expr) => {{
                let mut connection = $pool.acquire().await.expect("a connection");
                let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.text.clone()));
                for arg in &sql.args {
                    query = match arg {
                        Value::I64(number) => query.bind(*number),
                        Value::Null(_) => query.bind(None::<i64>),
                        other => panic!("the fixture binds only bigints, got {other:?}"),
                    };
                }
                query
                    .fetch_all(&mut *connection)
                    .await
                    .unwrap_or_else(|error| panic!("{error}\n  statement: {}", sql.text))
                    .iter()
                    .map(|row| (row.get::<i64, _>(0), row.get::<Option<i64>, _>(1)))
                    .collect()
            }};
        }

        match db.postgres_pool() {
            Some(pool) => fetch!(pool),
            None => fetch!(db.sqlite_pool().expect("one backend or the other")),
        }
    }

    /// Reads one page, in the query's forward order however it ran.
    ///
    /// Returns the rows, whether there is another page, and the ordering key
    /// the next cursor has to be minted for.
    async fn page_once(
        db: &Db,
        order: &[OrderTerm],
        cursor: Option<Cursor>,
        limit: u32,
        direction: PageDirection,
    ) -> (Vec<(i64, Option<i64>)>, bool, OrderingKey) {
        let mut query = Select::<Item>::new();
        for term in order {
            query = query.order_by(term.clone());
        }
        let mut page = query
            .paginate(cursor, limit)
            .signed_with(CursorCodec::new(SECRET));
        if direction.reverses_the_order() {
            page = page.backward();
        }

        let key = page.ordering_key().expect("a keyset");
        let statement = page
            .to_select()
            .expect("a plan")
            .to_statement()
            .expect("a statement");
        let sql = Executor::handle(db).build(&statement).expect("rendered");

        let rows = run(db, &sql).await;
        let more = rows.len() > limit as usize;
        let mut kept = rows[..rows.len().min(limit as usize)].to_vec();
        if direction.reverses_the_order() {
            kept.reverse();
        }
        (kept, more, key)
    }

    /// The sort-key tuple of one row, in the key's own order.
    fn key_values(key: &OrderingKey, row: (i64, Option<i64>)) -> Vec<Value> {
        key.columns()
            .iter()
            .map(|term| {
                if term.starts_with("kp_items.id ") {
                    Value::I64(row.0)
                } else {
                    row.1.map_or(Value::null(ValueKind::I64), Value::I64)
                }
            })
            .collect()
    }

    /// Seals a cursor the way `Paginated::fetch` does.
    fn mint(key: &OrderingKey, row: (i64, Option<i64>)) -> Cursor {
        PageCursor::new(key.fingerprint(), key_values(key, row))
            .seal(&CursorCodec::new(SECRET), Item::NAME)
            .expect("signable")
    }

    /// Pages through the whole table and returns the ids in the order seen,
    /// with the number of pages it took.
    async fn walk(db: &Db, order: &[OrderTerm], limit: u32) -> (Vec<i64>, usize) {
        let mut cursor: Option<Cursor> = None;
        let mut seen: Vec<i64> = Vec::new();
        let mut pages = 0_usize;

        loop {
            assert!(pages < 100, "the walk must terminate");
            let (rows, more, key) =
                page_once(db, order, cursor.take(), limit, PageDirection::Forward).await;
            pages += 1;
            seen.extend(rows.iter().map(|(id, _)| *id));

            let Some(last) = rows.last().copied() else {
                break;
            };
            if !more {
                break;
            }
            cursor = Some(mint(&key, last));
        }
        (seen, pages)
    }

    /// Every claim this module makes, against one backend.
    async fn acceptance(db: &Db) -> Result<()> {
        fixture(db).await?;

        // An unordered query paginates by the primary key alone, and sees
        // every row exactly once.
        let (seen, pages) = walk(db, &[], 7).await;
        assert_eq!(seen, (1..=ROWS).collect::<Vec<_>>());
        assert_eq!(pages, 4, "25 rows in pages of 7");

        // A nullable column with ties, ascending: NULLS LAST, and the primary
        // key breaks every tie. This is the case a naive `WHERE col > $1`
        // silently drops rows on.
        let (seen, _) = walk(db, &[SCORE.asc()], 4).await;
        assert_eq!(seen, ascending(), "ascending, NULLs last");

        // The same, descending: NULLS FIRST, and the tiebreaker follows the
        // last term's direction.
        let (seen, _) = walk(db, &[SCORE.desc()], 4).await;
        assert_eq!(seen, descending(), "descending, NULLs first");

        // A page of one still terminates and still sees everything once — the
        // boundary is on a tie or a NULL more often than not.
        let (seen, pages) = walk(db, &[SCORE.asc()], 1).await;
        assert_eq!(seen, ascending());
        assert_eq!(pages, ROWS as usize, "one row per page");

        // Paging backwards from the last row of page one returns the rows
        // before it, in forward order.
        let (first, more, key) =
            page_once(db, &[SCORE.asc()], None, 4, PageDirection::Forward).await;
        assert!(more, "25 rows do not fit in one page of 4");
        assert_eq!(first.len(), 4);
        let boundary = mint(&key, first[3]);
        let (back, _, _) = page_once(
            db,
            &[SCORE.asc()],
            Some(boundary),
            4,
            PageDirection::Backward,
        )
        .await;
        assert_eq!(
            back.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            first[..3].iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            "strictly before the cursor, flipped back into forward order"
        );

        // A cursor minted for one sort does not open another.
        let (_, _, other) = page_once(db, &[SCORE.desc()], None, 4, PageDirection::Forward).await;
        assert_ne!(key.fingerprint(), other.fingerprint());

        // Offset pagination over the same table lands on the same rows.
        let planned = Select::<Item>::new()
            .order_by(SCORE.asc())
            .paginate_offset(2, 4)
            .to_select();
        let sql = Executor::handle(db)
            .build(&planned.to_statement()?)
            .expect("rendered");
        let second: Vec<i64> = run(db, &sql).await.into_iter().map(|(id, _)| id).collect();
        assert_eq!(second, ascending()[4..8], "page two by offset");

        Ok(())
    }

    #[tokio::test]
    async fn keyset_pagination_holds_on_sqlite() {
        // A file rather than `:memory:`, because every connection in a pool
        // gets its own in-memory database and the fixture would vanish between
        // statements.
        let path =
            std::env::temp_dir().join(format!("moso-pagination-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("a SQLite database");
        let outcome = acceptance(&db).await;
        let _ = std::fs::remove_file(&path);
        outcome.expect("every criterion");
    }

    #[tokio::test]
    async fn keyset_pagination_holds_on_postgres() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!(
                "skipped: set DATABASE_URL to run the pagination acceptance suite against \
                 PostgreSQL (docker compose -f compose.test.yaml up -d)"
            );
            return;
        };
        let db = Db::connect_url(&url).await.expect("the test database");
        acceptance(&db).await.expect("every criterion");
    }
}
