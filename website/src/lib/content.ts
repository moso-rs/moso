import { SECTIONS, SECTION_BY_SLUG } from './nav';
import type { Doc, DocMetadata, Heading, NavSection } from './types';

/**
 * The content layer.
 *
 * Every markdown file under `src/content/docs` is pulled in eagerly and compiled
 * by the Vite plugin in `markdown/`, so this module holds the finished HTML for
 * the whole site. That is deliberate: the site is prerendered, so "eager" costs
 * build time rather than runtime, and it lets navigation, search fallbacks, the
 * sitemap and the previous/next links all derive from one in-memory list
 * instead of a second source of truth that can drift.
 */

interface CompiledDoc {
	metadata: DocMetadata;
	html: string;
	headings: Heading[];
	text: string;
	readingMinutes: number;
}

const modules = import.meta.glob<CompiledDoc>('/src/content/docs/**/*.md', { eager: true });

function slugFromPath(path: string): string {
	return path
		.replace('/src/content/docs/', '')
		.replace(/\.md$/, '')
		.replace(/\/index$/, '')
		.replace(/^index$/, '');
}

function build(): Doc[] {
	const docs: Doc[] = [];

	for (const [path, mod] of Object.entries(modules)) {
		const slug = slugFromPath(path);
		const metadata = mod.metadata ?? ({} as DocMetadata);

		if (!metadata.title) {
			throw new Error(`${path} has no \`title\` in its frontmatter.`);
		}
		if (!metadata.description) {
			throw new Error(`${path} has no \`description\` in its frontmatter.`);
		}

		docs.push({
			slug,
			url: slug ? `/docs/${slug}` : '/docs',
			section: slug.split('/')[0] ?? '',
			metadata,
			html: mod.html,
			headings: mod.headings ?? [],
			text: mod.text ?? '',
			readingMinutes: mod.readingMinutes ?? 1
		});
	}

	return docs;
}

export const docs: Doc[] = build();

export const docByUrl = new Map(docs.map((d) => [d.url, d]));

function comparePages(a: Doc, b: Doc): number {
	const ao = a.metadata.order ?? Number.MAX_SAFE_INTEGER;
	const bo = b.metadata.order ?? Number.MAX_SAFE_INTEGER;
	if (ao !== bo) return ao - bo;
	return a.metadata.title.localeCompare(b.metadata.title);
}

/**
 * The sidebar tree: sections in the order declared in `nav.ts`, pages in the
 * order declared in their frontmatter. A section's own `index.md` is pulled out
 * as the section landing page rather than listed among its children.
 */
export const navigation: NavSection[] = SECTIONS.map((section) => {
	const inSection = docs.filter((d) => d.section === section.slug && !d.metadata.draft);
	const index = inSection.find((d) => d.slug === section.slug);
	const pages = inSection.filter((d) => d !== index).sort(comparePages);
	return { ...section, pages, index };
}).filter((s) => s.pages.length > 0 || s.index);

/**
 * Reading order across the whole site, which is what previous/next walks.
 * A section index comes before that section's pages.
 */
export const readingOrder: Doc[] = navigation.flatMap((s) => (s.index ? [s.index, ...s.pages] : s.pages));

export function neighbours(url: string): { prev?: Doc; next?: Doc } {
	const i = readingOrder.findIndex((d) => d.url === url);
	if (i === -1) return {};
	return {
		prev: i > 0 ? readingOrder[i - 1] : undefined,
		next: i < readingOrder.length - 1 ? readingOrder[i + 1] : undefined
	};
}

export function sectionOf(url: string): NavSection | undefined {
	const doc = docByUrl.get(url);
	if (!doc) return undefined;
	return navigation.find((s) => s.slug === doc.section);
}

export function sectionLabel(slug: string): string {
	return SECTION_BY_SLUG.get(slug)?.label ?? slug;
}

/** Every prerenderable docs path, used by the router and the sitemap. */
export const allDocUrls: string[] = docs.map((d) => d.url);
