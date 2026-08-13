/**
 * Vite plugin that compiles `.md` into a plain ES module.
 *
 * Importing a markdown file anywhere in the app yields:
 *
 *     import { metadata, html, headings, text, readingMinutes } from './page.md';
 *
 * which means `import.meta.glob` over the content directory is all the content
 * layer we need. There is no separate content API, no build step to remember,
 * and editing a file hot reloads the page that renders it.
 */

import { renderMarkdown } from './render.js';

export function markdown() {
	return {
		name: 'moso-markdown',
		enforce: 'pre',

		async transform(source, id) {
			if (!id.endsWith('.md')) return null;

			const url = siteUrlFor(id);
			const dir = siteDirFor(id);
			const base = process.env.BASE_PATH ?? '';
			const { metadata, html, headings, text, readingMinutes } = await renderMarkdown(source, {
				url,
				dir,
				base,
				label: id
			});

			const exports = {
				metadata,
				html,
				headings,
				text,
				readingMinutes,
				url
			};

			const code = Object.entries(exports)
				.map(([name, value]) => `export const ${name} = ${JSON.stringify(value)};`)
				.join('\n');

			return { code, map: null };
		}
	};
}

/**
 * Map a file on disk to the path it is served at, which the renderer needs in
 * order to resolve relative links between documents.
 *
 * `src/content/docs/guides/routing.md` becomes `/docs/guides/routing`, and an
 * `index.md` collapses onto its directory.
 */
function siteUrlFor(id) {
	const path = contentPath(id);
	if (path === null) return '/';
	return `/${path.replace(/\.md$/, '').replace(/\/index$/, '')}`;
}

/**
 * The site path of the directory the source file sits in.
 *
 * Relative links resolve against this rather than against the page's own URL,
 * because `start/index.md` is served at `/docs/start` while its neighbours are
 * at `/docs/start/*`. Using the URL would make `./installation.md` in the
 * section index resolve to `/docs/installation`.
 */
function siteDirFor(id) {
	const path = contentPath(id);
	if (path === null) return '/';
	const at = path.lastIndexOf('/');
	return at === -1 ? '/' : `/${path.slice(0, at)}`;
}

function contentPath(id) {
	const marker = '/src/content/';
	const normalised = id.replace(/\\/g, '/');
	const at = normalised.indexOf(marker);
	if (at === -1) return null;
	return normalised.slice(at + marker.length);
}
