import { readingOrder } from '$lib/content';
import { SITE } from '$lib/nav';

export const prerender = true;

/**
 * The whole documentation set as one plain text file, in reading order.
 *
 * The text comes from the markdown source rather than the rendered HTML, so it
 * carries no navigation chrome, no duplicated headings and no highlighting
 * markup.
 */
export function GET() {
	const parts: string[] = [
		`# ${SITE.name}: complete documentation`,
		'',
		SITE.description,
		'',
		'---',
		''
	];

	for (const doc of readingOrder) {
		parts.push(
			`## ${doc.metadata.title}`,
			`Source: ${SITE.url}${doc.url}/`,
			`Summary: ${doc.metadata.description}`,
			''
		);
		if (doc.metadata.status && doc.metadata.status !== 'shipped') {
			parts.push(`Status: ${doc.metadata.status}`, '');
		}
		parts.push(doc.text, '', '---', '');
	}

	return new Response(parts.join('\n'), {
		headers: { 'content-type': 'text/plain; charset=utf-8' }
	});
}
