# 33 — The Admin Panel

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Why it matters

Django's admin is the single most-cited reason teams choose Django. FastAPI's lack of one is a
documented gap. Cot shipped an admin without pagination, filtering, or search — and said so — which
is a useful negative example: **an admin without those three is a demo, not a tool.** They are v1
scope here, not v2.

## What it is

A server-rendered CRUD interface generated from `#[derive(Entity)]` metadata, mounted at `/admin`,
gated by `moso-authz`. No JavaScript build step: `maud` templates, HTMX for interactivity, a small
hand-written CSS file, all assets embedded in the binary.

The choice of "no JS toolchain" is deliberate and load-bearing. An admin that requires `npm` in the
build pipeline is an admin people disable.

## Registering models

```rust
// example — src/admin.rs
pub fn admin() -> AdminBuilder {
    Admin::new()
        .title("Shop")
        .model::<User>(|m| m
            .list_display([User::EMAIL, User::NAME, User::IS_ADMIN, User::CREATED_AT])
            .list_filter([Filter::boolean(User::IS_ADMIN), Filter::date_range(User::CREATED_AT)])
            .search([User::EMAIL, User::NAME])
            .ordering(User::CREATED_AT.desc())
            .readonly([User::ID, User::CREATED_AT, User::UPDATED_AT])
            .exclude([User::PASSWORD_HASH])          // never rendered, never editable
            .per_page(50)
            .action("Suspend", Perm::UsersSuspend, |ids, db| async move { … })
            .inline::<ApiKey>(Inline::tabular())     // edit related rows on the parent page
        )
        .model::<Post>(|m| m
            .list_display([Post::TITLE, Post::AUTHOR, Post::PUBLISHED_AT])
            .list_filter([Filter::relation(Post::AUTHOR), Filter::choice(Post::STATUS)])
            .search([Post::TITLE, Post::BODY])
            .prefetch([Post::AUTHOR])               // admin lists must not N+1 either
        )
        .dashboard(dashboard_widgets())
        .jobs()                                      // mount the jobs dashboard
        .flags()                                     // feature-flag editor
        .audit()                                     // audit-log browser
}
```

Zero-config also works: `.model::<User>(|m| m)` infers list columns (the first 5 scalar columns
plus timestamps), search over text columns, and filters over booleans, enums, dates, and FKs.

## Feature list (all v1)

| Feature | Detail |
| --- | --- |
| **List view** | server-side pagination (keyset, so page 500 is as fast as page 1), sorting on indexed columns, column selection |
| **Search** | across declared columns; Postgres full-text when a `tsvector` index exists, `ILIKE` otherwise; the admin says which it used |
| **Filters** | boolean, choice/enum, date range with presets, numeric range, relation (with a searchable picker), null/not-null, custom `Filter` impls |
| **Detail/edit** | form generated from column types; validation via the entity's `Schema` constraints; optimistic-locking conflict shown as a diff |
| **Create** | same form, with defaults |
| **Delete** | confirmation showing what will cascade, by querying the FK graph first |
| **Bulk actions** | select rows → run a registered action; permission-gated; runs as a job if it exceeds a threshold |
| **Inlines** | edit `has_many` children on the parent page (tabular or stacked) |
| **Relation widgets** | FK as a searchable select with pagination; M2M as a dual list |
| **Rich fields** | JSON editor with schema hints, markdown editor with preview, image upload with thumbnail, colour, duration, enum radio |
| **History/audit** | who changed what, when, field-level diff; revert to a previous version |
| **Export** | CSV/JSON of the current filtered query, streamed, run as a job past 10k rows |
| **Import** | CSV with a column-mapping step, dry-run diff, and per-row error reporting |
| **Dashboard** | configurable widgets: counts, time series, recent activity, health |
| **Jobs** | the dashboard from `32-jobs.md` |
| **Flags** | feature-flag editor with audit |
| **Impersonation** | "log in as" with a persistent banner and full audit; permission-gated |
| **Global search** | across registered models |
| **Dark mode, keyboard nav, responsive** | table switches to cards under 768 px |
| **i18n** | UI strings translatable; RTL-aware |
| **Accessibility** | WCAG 2.2 AA: labels, focus management, ARIA on HTMX swaps, no colour-only state |

## Permissions

Per-model permissions are derived automatically: registering `User` implies
`users.view`/`create`/`update`/`delete` exist and gate the corresponding admin actions. They are
added to the permission registry so they appear in the role editor and the audit.

Field-level permissions: `.field_perm(User::IS_ADMIN, Perm::UsersGrantAdmin)` hides or disables the
field for actors lacking it. Row-level: the admin honours `ScopedPolicy` (`31-authorization.md`), so
a tenant-scoped admin user sees only their tenant's rows — with no extra configuration, because it
is the same query filter the API uses.

## Performance rules

The admin is where frameworks quietly ship N+1s and 30-second list pages.

1. List queries use keyset pagination and never `COUNT(*)` on an unindexed predicate. Totals are
   approximate (`reltuples`) above a configurable row threshold, and the UI says "about 4.2M".
2. Every FK rendered in a list is prefetched. The admin's own test suite asserts statement counts.
3. Search on a column with no index emits a **visible warning in the admin UI** in dev, naming the
   migration to add. Making performance advice visible where the mistake is made is far more
   effective than documenting it.
4. Exports stream and never buffer the full result set.
5. A slow admin query logs with the same slow-query machinery as the app.

## Customisation

Escape hatches, in increasing order of power:
1. **Config**: the `.model::<T>()` builder covers the common 90%.
2. **Custom column/field renderers**: `impl AdminField for MyType`.
3. **Custom pages**: `.page("/admin/reports", handler)` — an ordinary Moso handler rendering into
   the admin layout.
4. **Template override**: supply a directory of `maud`/`minijinja` templates that shadow the
   built-ins.
5. **Headless**: `.api_only()` exposes the admin's metadata and CRUD as a JSON API so a team can
   build their own frontend against it, which is what larger teams eventually want.

## Security

- Mounted at a configurable path; `admin.path = "/manage"` supported and recommended in the docs.
- Requires `Perm::AdminAccess`; disabled entirely by default in the `production` profile unless
  `admin.enabled = true` is set explicitly.
- CSRF on every mutating form. Strict CSP with nonces; no inline event handlers.
- Re-authentication required for destructive bulk actions and for impersonation.
- Every write is audited with the actor, the before/after diff, and the request id.
- Rate-limited login, and the admin never exposes the API's OpenAPI document.

## What we are NOT building

- A page builder or CMS.
- A no-code workflow engine.
- A charting library — the dashboard widgets take a query and render a small SVG; anything richer
  belongs in a real BI tool and the docs say so.

## Acceptance criteria (WP-20)

1. `moso new --admin` produces a working admin over the generated `User` entity with zero admin
   configuration.
2. A list of 1M rows paginates with p95 < 100 ms at any offset (keyset), asserted in a benchmark.
3. Admin list views issue a bounded number of statements regardless of row count (query counter
   test with 3 FKs).
4. Field-level and row-level permissions are enforced on render **and** on submit (a crafted POST
   cannot set a field the actor may not edit) — tested adversarially.
5. Every mutating action produces an audit entry with a field-level diff.
6. The UI passes an automated WCAG 2.2 AA check and is usable at 375 px width.
7. No outbound network requests; all assets embedded (egress-blocked test).
8. Import/export round-trips 100k rows without exceeding 100 MB RSS.
