declare global {
	namespace App {}

	interface Window {
		/** Injected by Pagefind's own bundle, loaded on demand by the search dialog. */
		__pagefind__?: unknown;
	}
}

declare module '*.md' {
	import type { DocMetadata, Heading } from '$lib/types';
	export const metadata: DocMetadata;
	export const html: string;
	export const headings: Heading[];
	export const text: string;
	export const readingMinutes: number;
	export const url: string;
}

export {};
