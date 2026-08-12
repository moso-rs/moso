import { describe, expect, it } from 'vitest';
import { renderMarkdown, splitFrontmatter } from '../markdown/render.js';

const at = (source: string, url = '/docs/guides/routing', base = '') =>
	renderMarkdown(source, { url, base });

describe('frontmatter', () => {
	it('separates YAML frontmatter from the body', () => {
		const { data, body } = splitFrontmatter('---\ntitle: Routing\norder: 2\n---\n\nHello.\n');
		expect(data).toEqual({ title: 'Routing', order: 2 });
		expect(body.trim()).toBe('Hello.');
	});

	it('leaves a document without frontmatter alone', () => {
		const { data, body } = splitFrontmatter('Just text.\n');
		expect(data).toEqual({});
		expect(body).toBe('Just text.\n');
	});
});

describe('headings', () => {
	it('collects h2 and h3 with slugged ids', async () => {
		const { headings } = await at('## Set up the router\n\n### Path parameters\n\n#### Ignored\n');
		expect(headings).toEqual([
			{ depth: 2, id: 'set-up-the-router', text: 'Set up the router' },
			{ depth: 3, id: 'path-parameters', text: 'Path parameters' }
		]);
	});

	it('does not let the anchor character leak into the heading text', async () => {
		const { headings } = await at('## Errors\n');
		expect(headings[0].text).toBe('Errors');
	});
});

describe('links', () => {
	it('resolves a sibling markdown link to a clean URL', async () => {
		const { html } = await at('[see](./openapi.md)');
		expect(html).toContain('href="/docs/guides/openapi/"');
	});

	it('resolves a parent relative link', async () => {
		const { html } = await at('[start](../start/quick-start.md)');
		expect(html).toContain('href="/docs/start/quick-start/"');
	});

	it('keeps the fragment when one is present', async () => {
		const { html } = await at('[slots](./middleware.md#slots)');
		expect(html).toContain('href="/docs/guides/middleware/#slots"');
	});

	it('resolves relative links in a section index against its own directory', async () => {
		// `start/index.md` is served at `/docs/start`, so resolving against the
		// URL's parent would send `./installation.md` to `/docs/installation`.
		const { html } = await renderMarkdown('[install](./installation.md) [guides](../guides/index.md)', {
			url: '/docs/start',
			dir: '/docs/start'
		});
		expect(html).toContain('href="/docs/start/installation/"');
		expect(html).toContain('href="/docs/guides/"');
	});

	it('applies the deployment base path', async () => {
		const { html } = await at('[x](/docs/reference/cli.md)', '/docs/guides/routing', '/moso');
		expect(html).toContain('href="/moso/docs/reference/cli/"');
	});

	it('marks external links and makes them safe', async () => {
		const { html } = await at('[axum](https://docs.rs/axum)');
		expect(html).toContain('rel="noopener noreferrer"');
		expect(html).toContain('data-external');
	});

	it('leaves in-page anchors untouched', async () => {
		const { html } = await at('[jump](#later)');
		expect(html).toContain('href="#later"');
	});
});

describe('callouts', () => {
	it('turns a GitHub alert into a callout with a label', async () => {
		const { html } = await at('> [!WARNING]\n> This drops the table.\n');
		expect(html).toContain('class="callout callout-warning"');
		expect(html).toContain('role="alert"');
		expect(html).toContain('Warning');
		expect(html).toContain('This drops the table.');
	});

	it('uses a short custom title when one is given', async () => {
		const { html } = await at('> [!NOTE] Requires Postgres\n> Set DATABASE_URL first.\n');
		expect(html).toContain('Requires Postgres');
		expect(html).not.toContain('>Note<');
	});

	it('leaves an ordinary blockquote as a blockquote', async () => {
		const { html } = await at('> Just a quote.\n');
		expect(html).toContain('<blockquote>');
		expect(html).not.toContain('callout');
	});
});

describe('code', () => {
	it('highlights Rust and exposes both theme variables', async () => {
		const { html } = await at('```rust\nlet x = 1;\n```\n');
		expect(html).toContain('--sx-light');
		expect(html).toContain('--sx-dark');
		expect(html).toContain('shiki');
	});

	it('carries a fence title through to the markup', async () => {
		const { html } = await at('```rust title="src/main.rs"\nfn main() {}\n```\n');
		expect(html).toContain('data-title="src/main.rs"');
	});

	it('falls back rather than failing on an unknown language', async () => {
		const { html } = await at('```nosuchlang\nplain\n```\n');
		expect(html).toContain('plain');
	});
});

describe('text extraction', () => {
	it('keeps code in the plain text, because here the code is the content', async () => {
		const { text } = await at('Intro.\n\n```rust\nlet answer = 42;\n```\n');
		expect(text).toContain('Intro.');
		expect(text).toContain('let answer = 42;');
	});

	it('estimates reading time from word count', async () => {
		const { readingMinutes } = await at(`${'word '.repeat(440)}`);
		expect(readingMinutes).toBe(2);
	});
});

describe('tables', () => {
	it('renders GitHub flavoured tables', async () => {
		const { html } = await at('| a | b |\n| --- | --- |\n| 1 | 2 |\n');
		expect(html).toContain('<table>');
		expect(html).toContain('<th>a</th>');
	});
});
