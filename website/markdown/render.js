/**
 * The markdown pipeline.
 *
 * Every `.md` file under `src/content` is compiled to HTML once, at build time,
 * by this module. Nothing here runs in the browser: the site ships rendered
 * HTML and no markdown parser, no highlighter and no runtime formatting cost.
 *
 * The pipeline is deliberately explicit rather than a framework preset, because
 * the docs rely on four behaviours a preset would not give us: GitHub style
 * alert blockquotes, code fences that carry a filename, heading extraction for
 * the table of contents, and internal links written as relative `.md` paths so
 * that the content stays readable in a plain editor and on GitHub.
 */

import { unified } from 'unified';
import remarkParse from 'remark-parse';
import remarkGfm from 'remark-gfm';
import remarkRehype from 'remark-rehype';
import rehypeRaw from 'rehype-raw';
import rehypeSlug from 'rehype-slug';
import rehypeAutolinkHeadings from 'rehype-autolink-headings';
import rehypeStringify from 'rehype-stringify';
import rehypeShiki from '@shikijs/rehype';
import { visit } from 'unist-util-visit';
import { toString as hastToString } from 'hast-util-to-string';
import { parse as parseYaml } from 'yaml';

import { SYNTAX_DARK, SYNTAX_LIGHT } from './syntax-themes.js';

/** Alert keywords understood in blockquotes, mapped to a callout variant. */
const ALERT_KINDS = {
	NOTE: { variant: 'note', label: 'Note' },
	TIP: { variant: 'tip', label: 'Tip' },
	IMPORTANT: { variant: 'important', label: 'Important' },
	WARNING: { variant: 'warning', label: 'Warning' },
	CAUTION: { variant: 'caution', label: 'Caution' }
};

const FRONTMATTER = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/;

/**
 * Split YAML frontmatter off the top of a document.
 *
 * The parse is wrapped because the common authoring mistake here is an
 * unquoted value containing a colon, and YAML's own error reports a line and
 * column inside a fragment with no idea which file it came from. Naming the
 * file and the fix turns a two minute hunt into a one line correction.
 *
 * @param {string} source
 * @param {string} [label] path to name in an error message
 * @returns {{ data: Record<string, unknown>, body: string }}
 */
export function splitFrontmatter(source, label = 'markdown file') {
	const match = source.match(FRONTMATTER);
	if (!match) return { data: {}, body: source };

	let data;
	try {
		data = parseYaml(match[1]) ?? {};
	} catch (cause) {
		throw new Error(
			`${label}: the frontmatter is not valid YAML. ${cause.message}\n` +
				'A value containing a colon has to be quoted, for example:\n' +
				"  description: 'Wire the battery: a backend and an extractor.'",
			{ cause }
		);
	}

	return { data, body: source.slice(match[0].length) };
}

/**
 * Turn GitHub style alert blockquotes into callout elements.
 *
 * Input:
 *     > [!WARNING]
 *     > Running this drops the table.
 *
 * Output: a `div.callout.callout-warning` with a labelled header. The syntax is
 * used rather than a custom `:::` fence so the same file still renders as an
 * alert when read on GitHub.
 */
function remarkCallouts() {
	/**
	 * Pull the `[!KIND]` marker off the front of a paragraph, if there is one.
	 *
	 * The marker arrives in one of two shapes. Usually it is plain text, but
	 * `[!NOTE]` is also valid shortcut link reference syntax, so remark parses
	 * it as a `linkReference` whenever the surrounding text makes that reading
	 * possible. Both have to be handled or the callout silently stays a
	 * blockquote.
	 */
	function takeMarker(paragraph) {
		const first = paragraph.children?.[0];
		if (!first) return null;

		if (first.type === 'text') {
			const match = first.value.match(/^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\]/);
			if (!match) return null;
			first.value = first.value.slice(match[0].length);
			return match[1];
		}

		if (first.type === 'linkReference') {
			const inner = first.children?.[0];
			if (inner?.type !== 'text') return null;
			const match = inner.value.match(/^!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)$/);
			if (!match) return null;
			paragraph.children.shift();
			return match[1];
		}

		return null;
	}

	/** Text left on the marker's own line, which is read as a custom title. */
	function takeTrailingText(paragraph) {
		const next = paragraph.children[0];
		if (!next || next.type !== 'text') return '';

		const newline = next.value.indexOf('\n');
		if (newline === 0) {
			next.value = next.value.slice(1);
			return '';
		}
		if (newline === -1) {
			// Everything left is on the marker line. Only treat it as a title
			// when nothing else follows in the paragraph.
			if (paragraph.children.length > 1) return '';
			paragraph.children.shift();
			return next.value.trim();
		}
		const title = next.value.slice(0, newline).trim();
		next.value = next.value.slice(newline + 1);
		return title;
	}

	return (tree) => {
		visit(tree, 'blockquote', (node) => {
			const first = node.children?.[0];
			if (!first || first.type !== 'paragraph') return;

			const key = takeMarker(first);
			if (!key) return;

			const kind = ALERT_KINDS[key];
			const trailing = takeTrailingText(first);

			if (first.children.length === 0) node.children.shift();

			// A short phrase after the marker reads as a title. A full sentence
			// reads as body text, so it stays in the body.
			const useCustomTitle = trailing.length > 0 && trailing.length <= 48;
			if (trailing && !useCustomTitle) {
				node.children.unshift({
					type: 'paragraph',
					children: [{ type: 'text', value: trailing }]
				});
			}

			node.data = {
				hName: 'div',
				hProperties: {
					className: ['callout', `callout-${kind.variant}`],
					role: kind.variant === 'warning' || kind.variant === 'caution' ? 'alert' : 'note'
				}
			};

			node.children.unshift({
				type: 'paragraph',
				data: { hProperties: { className: ['callout-title'] } },
				children: [
					{
						type: 'text',
						value: useCustomTitle ? trailing : kind.label
					}
				]
			});
		});
	};
}

/**
 * Rewrite links so that content authored as portable markdown resolves to the
 * site's clean URLs.
 *
 * `./routing.md` next to `guides/openapi.md` becomes `/docs/guides/routing/`.
 * Absolute site paths gain the configured base path. External links are marked
 * so the stylesheet can flag them and so they open safely.
 *
 * @param {{ dir: string, base: string }} options
 */
function rehypeLinks({ dir, base }) {
	return (tree) => {
		visit(tree, 'element', (node) => {
			if (node.tagName !== 'a') return;
			const href = node.properties?.href;
			if (typeof href !== 'string' || href.length === 0) return;

			if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith('//')) {
				if (/^https?:/i.test(href)) {
					node.properties.rel = 'noopener noreferrer';
					node.properties['data-external'] = '';
				}
				return;
			}

			if (href.startsWith('#')) return;

			const [rawPath, hash] = splitHash(href);
			let resolved;

			if (rawPath.startsWith('/')) {
				resolved = rawPath;
			} else {
				// Resolve against the directory holding the source file. This is
				// not the same as the directory of the page's URL: a section's
				// `index.md` lives at `content/docs/start/` but is served at
				// `/docs/start`, so deriving the directory from the URL would
				// resolve every relative link in a section index one level too
				// high.
				resolved = normalisePath(`${dir}/${rawPath}`);
			}

			resolved = resolved.replace(/(?:\/index)?\.md$/, '');
			if (resolved === '') resolved = '/';
			if (!resolved.endsWith('/')) resolved = `${resolved}/`;

			node.properties.href = `${base}${resolved}${hash}`;
		});
	};
}

function splitHash(href) {
	const i = href.indexOf('#');
	return i === -1 ? [href, ''] : [href.slice(0, i), href.slice(i)];
}

function normalisePath(path) {
	const out = [];
	for (const part of path.split('/')) {
		if (part === '' || part === '.') continue;
		if (part === '..') out.pop();
		else out.push(part);
	}
	return `/${out.join('/')}`;
}

/**
 * Collect `h2` and `h3` headings for the table of contents.
 *
 * Runs after `rehype-slug`, so every heading already carries the id the anchor
 * link points at.
 *
 * @param {{ into: Array<{ depth: number, id: string, text: string }> }} options
 */
function rehypeCollectHeadings({ into }) {
	return (tree) => {
		visit(tree, 'element', (node) => {
			if (node.tagName !== 'h2' && node.tagName !== 'h3') return;
			const id = node.properties?.id;
			if (typeof id !== 'string') return;
			into.push({
				depth: node.tagName === 'h2' ? 2 : 3,
				id,
				text: hastToString(node).replace(/\s*#$/, '').trim()
			});
		});
	};
}

/**
 * Extract readable plain text, used for search excerpts, reading time and the
 * `llms-full.txt` corpus. Code blocks are kept because for this site they are
 * a large part of the meaning.
 */
function extractText(tree) {
	const parts = [];
	visit(tree, (node) => {
		if (node.type === 'text') parts.push(node.value);
		else if (node.type === 'inlineCode') parts.push(node.value);
		else if (node.type === 'code') parts.push(node.value);
	});
	return parts.join(' ').replace(/\s+/g, ' ').trim();
}

/**
 * Carry the fence's info string across `rehype-raw`.
 *
 * `remark-rehype` puts the text after the language (`rust title="x"`) in
 * `code.data.meta`, but `rehype-raw` reparses the tree through an HTML parser
 * and `data` is not expressible in HTML, so it is dropped. Copying it into a
 * real attribute first survives that round trip. The attribute is named
 * `metastring` because Shiki already falls back to reading exactly that, and
 * because a `data-` prefixed name would come back from the parser camel cased.
 */
function rehypeStashMeta() {
	return (tree) => {
		visit(tree, 'element', (node) => {
			if (node.tagName !== 'code') return;
			const meta = node.data?.meta;
			if (typeof meta === 'string' && meta.length > 0) {
				node.properties = { ...node.properties, metastring: meta };
			}
		});
	};
}

/** Parse the text after the language on a fence: `rust title="src/main.rs"`. */
function parseMeta(meta) {
	const props = {};
	if (!meta) return props;

	const title = meta.match(/title="([^"]+)"|title='([^']+)'/);
	if (title) props['data-title'] = title[1] ?? title[2];

	if (/\bnoframe\b/.test(meta)) props['data-noframe'] = '';
	if (/\bnocopy\b/.test(meta)) props['data-nocopy'] = '';

	return props;
}

/**
 * Compile one markdown document.
 *
 * @param {string} source raw file contents, frontmatter included
 * @param {object} options
 * @param {string} options.url site path this document is served at.
 *   Example: `/docs/guides/routing`.
 * @param {string} [options.dir] site path of the directory holding the source
 *   file, which is what relative links resolve against. Defaults to the URL's
 *   parent, which is correct for every page except a directory's `index.md`.
 * @param {string} [options.base] base path the site is deployed under
 * @param {string} [options.label] path to name in error messages
 */
export async function renderMarkdown(
	source,
	{ url, dir = url.slice(0, url.lastIndexOf('/')), base = '', label = url }
) {
	const { data, body } = splitFrontmatter(source, label);
	const headings = [];

	let text = '';
	const capture = () => (tree) => {
		text = extractText(tree);
	};

	const file = await unified()
		.use(remarkParse)
		.use(remarkGfm)
		.use(capture)
		.use(remarkCallouts)
		.use(remarkRehype, { allowDangerousHtml: true })
		.use(rehypeStashMeta)
		.use(rehypeRaw)
		.use(rehypeSlug)
		.use(rehypeAutolinkHeadings, {
			behavior: 'append',
			properties: {
				className: ['heading-anchor'],
				ariaHidden: 'true',
				tabIndex: -1
			},
			content: { type: 'text', value: '#' }
		})
		.use(rehypeCollectHeadings, { into: headings })
		.use(rehypeLinks, { dir, base })
		.use(rehypeShiki, {
			themes: { light: SYNTAX_LIGHT, dark: SYNTAX_DARK },
			defaultColor: false,
			cssVariablePrefix: '--sx-',
			parseMetaString: parseMeta,
			// `parseMetaString` only feeds Shiki's `meta` bag. Moving those keys
			// onto the rendered `<pre>` is a transformer's job, and it has to
			// happen here because Shiki replaces the element wholesale.
			transformers: [
				{
					name: 'moso-fence-meta',
					pre(node) {
						const meta = this.options.meta ?? {};
						for (const key of ['data-title', 'data-noframe', 'data-nocopy']) {
							if (key in meta) node.properties[key] = meta[key];
						}
						if (this.options.lang) node.properties['data-language'] = this.options.lang;
					}
				}
			],
			fallbackLanguage: 'text',
			langs: [
				'rust',
				'toml',
				'yaml',
				'json',
				'bash',
				'sql',
				'html',
				'javascript',
				'typescript',
				'diff',
				'text',
				'ini',
				'dockerfile'
			]
		})
		.use(rehypeStringify, { allowDangerousHtml: true })
		.process(body);

	const words = text.split(/\s+/).filter(Boolean).length;

	return {
		metadata: data,
		html: String(file),
		headings,
		text,
		readingMinutes: Math.max(1, Math.round(words / 220))
	};
}
