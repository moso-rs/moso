import adapter from '@sveltejs/adapter-static';

/**
 * The site is fully static: every route is prerendered at build time and the
 * output is a folder of HTML you can drop on any static host. `trailingSlash`
 * is `always` so each page lands as `<route>/index.html`, which is what makes
 * clean URLs work on hosts that do no rewriting of their own (GitHub Pages,
 * S3, plain nginx).
 *
 * `BASE_PATH` exists for project-page hosting, where the site is served from a
 * subdirectory rather than the domain root.
 *
 * @type {import('@sveltejs/kit').Config}
 */
const config = {
	kit: {
		adapter: adapter({
			pages: 'build',
			assets: 'build',
			fallback: '404.html',
			precompress: false,
			strict: true
		}),
		paths: {
			base: process.env.BASE_PATH ?? ''
		},
		prerender: {
			handleHttpError: 'fail',
			handleMissingId: 'fail'
		},
		alias: {
			$content: 'src/content'
		}
	}
};

export default config;
