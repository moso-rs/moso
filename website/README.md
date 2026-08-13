# The Moso documentation site

A static site: SvelteKit with `adapter-static`, prerendered at build time into a folder of HTML
with no server behind it.

```sh
pnpm install
pnpm dev              # http://localhost:5173
pnpm build            # -> build/, including the search index
pnpm preview          # serve build/ exactly as it will be served in production
```

## What is where

| Path | Contents |
| --- | --- |
| `src/content/docs/**` | Every documentation page, as markdown. This is the only place content lives. |
| `src/lib/nav.ts` | Section order and labels, primary navigation, site metadata. |
| `src/lib/content.ts` | The content layer: globs the markdown, builds the sidebar tree and the reading order. |
| `src/lib/components/` | Navigation, sidebar, table of contents, search, theme toggle, prose renderer. |
| `src/routes/` | The landing page, the docs routes, `sitemap.xml`, `llms.txt`, `llms-full.txt`. |
| `src/styles/` | `tokens.css` holds every colour and size. `app.css` is chrome, `prose.css` is content. |
| `markdown/` | The build time markdown pipeline: remark and rehype plugins, Shiki themes. |
| `scripts/lint-content.mjs` | The content gate that CI runs. |
| `tests/` | Unit tests for the pipeline, plus invariants over the content set. |

## Adding a page

Create a markdown file under `src/content/docs/<section>/`. Nothing else needs editing: the sidebar,
the previous and next links, the sitemap, the search index and `llms.txt` all derive from the file
and its frontmatter.

```markdown
---
title: Rate limiting
description: Apply a rate limit to a route, choose a backend, and decide what happens when a caller exceeds it.
order: 21
status: shipped
---

Prose starts here. Do not write an `h1`; the template renders the title.
```

| Field | Required | Notes |
| --- | --- | --- |
| `title` | yes | The `h1`, the sidebar entry and the search result title. |
| `description` | yes | One sentence, under 200 characters, ending in a full stop. |
| `order` | no | Position in the sidebar. Pages without one sort last, alphabetically. |
| `status` | no | `shipped` (default), `partial` or `planned`. Anything but `shipped` renders a banner. |
| `sidebar` | no | A shorter label, for when the title does not fit. |
| `related` | no | Site paths rendered as a related list at the foot of the page. |

Link between pages with relative markdown paths (`../guides/errors.md`) so the content still reads
correctly in an editor and on GitHub. The build rewrites them to clean URLs.

## Markdown extensions

Fenced blocks take a filename, which renders as a title bar:

    ```rust title="src/routes/posts.rs"

GitHub alert blockquotes become callouts. `NOTE`, `TIP`, `IMPORTANT`, `WARNING` and `CAUTION` are
supported:

```markdown
> [!WARNING]
> This rewrites the table in place.
```

## House rules, enforced

`pnpm lint:content` and `pnpm test` fail the build on any of these:

- an em dash, or an en dash used as punctuation, anywhere in the content
- a page with no title or no description, or a description that is too long
- an internal link that does not resolve to a real page
- a fenced code block with no language
- a body that opens with an `h1`
- two pages in one section claiming the same `order`

Prerendering is configured with `handleHttpError: 'fail'`, so a broken link in a Svelte component
fails the build too.

## Search

Search is [Pagefind](https://pagefind.app), which indexes the built HTML after the site is
generated and serves chunked indexes fetched on demand. It therefore only works against a build:

```sh
pnpm build && pnpm preview
```

In `pnpm dev` the search dialog opens and says so rather than failing silently.

## Deployment

`.github/workflows/website.yml` lints, type checks, tests, builds and publishes to GitHub Pages on
every push to `main`. Set `BASE_PATH` when the site is served from a subdirectory rather than a
domain root; the workflow already does this for project pages.
