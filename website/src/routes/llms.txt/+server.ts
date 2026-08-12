import { navigation } from '$lib/content';
import { SITE } from '$lib/nav';

export const prerender = true;

/**
 * A machine readable index of the documentation, following the llms.txt
 * convention: a short description of the project followed by grouped links.
 *
 * Assistants are a real audience for framework documentation now, and they read
 * whatever they are pointed at. Giving them an accurate index costs one file
 * and is a lot better than letting them guess an API from a blog post.
 */
export function GET() {
	const lines: string[] = [
		`# ${SITE.name}`,
		'',
		`> ${SITE.description}`,
		'',
		'Moso applications are written against one facade crate, `moso`. A single type definition',
		'drives request parsing, validation, serialisation and the OpenAPI document, and the data',
		'layer, authentication, authorisation, background jobs, cache, mail and object storage ship',
		'with the framework rather than being assembled from the ecosystem.',
		'',
		`Full text of every page: ${SITE.url}/llms-full.txt`,
		''
	];

	for (const section of navigation) {
		lines.push(`## ${section.label}`, '');
		if (section.index) {
			lines.push(`- [${section.index.metadata.title}](${SITE.url}${section.index.url}/): ${section.index.metadata.description}`);
		}
		for (const doc of section.pages) {
			lines.push(`- [${doc.metadata.title}](${SITE.url}${doc.url}/): ${doc.metadata.description}`);
		}
		lines.push('');
	}

	return new Response(lines.join('\n'), {
		headers: { 'content-type': 'text/plain; charset=utf-8' }
	});
}
