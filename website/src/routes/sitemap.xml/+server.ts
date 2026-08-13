import { docs } from '$lib/content';
import { SITE } from '$lib/nav';

export const prerender = true;

const STATIC_PATHS = ['/', '/docs'];

export function GET() {
	const urls = [...STATIC_PATHS, ...docs.filter((d) => d.slug !== '').map((d) => d.url)];

	const body = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${urls
	.map((path) => {
		const loc = path === '/' ? `${SITE.url}/` : `${SITE.url}${path}/`;
		const priority = path === '/' ? '1.0' : path.split('/').length <= 3 ? '0.8' : '0.6';
		return `  <url>\n    <loc>${loc}</loc>\n    <priority>${priority}</priority>\n  </url>`;
	})
	.join('\n')}
</urlset>
`;

	return new Response(body, {
		headers: { 'content-type': 'application/xml' }
	});
}
