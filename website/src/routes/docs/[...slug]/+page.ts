import { error } from '@sveltejs/kit';
import { docByUrl, docs, neighbours, sectionOf } from '$lib/content';
import type { PageLoad } from './$types';

/**
 * Prerender targets. Every documentation page is a file in `src/content/docs`,
 * so the list of routes is the list of files, and adding a page needs no change
 * anywhere else.
 *
 * `/docs` itself is excluded: it has its own route with a section grid rather
 * than a markdown body.
 */
export function entries() {
	return docs.filter((doc) => doc.slug !== '').map((doc) => ({ slug: doc.slug }));
}

export const load: PageLoad = ({ params }) => {
	const url = `/docs/${params.slug}`.replace(/\/$/, '');
	const doc = docByUrl.get(url);

	if (!doc) {
		error(404, `No documentation page at ${url}`);
	}

	return {
		doc,
		section: sectionOf(url),
		...neighbours(url)
	};
};
