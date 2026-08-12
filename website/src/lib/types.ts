/** Shape of the frontmatter every documentation page carries. */
export interface DocMetadata {
	/** Page title, rendered as the `h1` and used in navigation. */
	title: string;
	/** One sentence summary. Used in search results, meta tags and section indexes. */
	description: string;
	/**
	 * Sort position inside the section. Pages without one sort last,
	 * alphabetically.
	 */
	order?: number;
	/**
	 * How much of what this page describes actually exists in the workspace.
	 * `shipped` is the default and needs no badge.
	 */
	status?: 'shipped' | 'partial' | 'planned';
	/** Optional shorter label for the sidebar when the title is long. */
	sidebar?: string;
	/** Related pages, as site paths, rendered at the foot of the page. */
	related?: string[];
	/** Hide from navigation while still building the page. */
	draft?: boolean;
}

export interface Heading {
	depth: 2 | 3;
	id: string;
	text: string;
}

export interface Doc {
	/** Path segments below `content/docs`, for example `guides/routing`. */
	slug: string;
	/** Absolute site path, without the base prefix and without a trailing slash. */
	url: string;
	/** First path segment, which is the section this page belongs to. */
	section: string;
	metadata: DocMetadata;
	html: string;
	headings: Heading[];
	text: string;
	readingMinutes: number;
}

export interface SectionDefinition {
	slug: string;
	label: string;
	/** Shown on the docs landing page and in the section header. */
	blurb: string;
}

export interface NavSection extends SectionDefinition {
	pages: Doc[];
	index?: Doc;
}
