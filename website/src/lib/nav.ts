import type { SectionDefinition } from './types';

/**
 * The order sections appear in the sidebar and on the docs landing page.
 *
 * The split follows the shape most technical documentation converges on: learn
 * it, do a specific thing with it, look something up, understand why it works
 * that way. A page that does not obviously belong to one of these is usually a
 * page that is trying to do two jobs at once.
 */
export const SECTIONS: SectionDefinition[] = [
	{
		slug: 'start',
		label: 'Getting started',
		blurb: 'Install Moso, serve your first endpoint, and see what the framework generates for you.'
	},
	{
		slug: 'guides',
		label: 'Guides',
		blurb: 'One task per page. Every feature the framework ships, with the code you would actually write.'
	}
];

export const SECTION_BY_SLUG = new Map(SECTIONS.map((s) => [s.slug, s]));

/** Top level site navigation. */
export const PRIMARY_NAV = [
	{ label: 'Docs', href: '/docs/start' },
	{ label: 'Guides', href: '/docs/guides' }
];

export const SITE = {
	name: 'Moso',
	tagline: 'The batteries-included Rust web framework',
	description:
		'Moso is a model-driven Rust web framework. One type definition drives parsing, validation, serialisation and OpenAPI, and the ORM, auth, jobs, cache, mail and storage are already in the box.',
	url: 'https://lowsbarrel.github.io/moso',
	repo: 'https://github.com/lowsbarrel/moso',
	editBase: 'https://github.com/lowsbarrel/moso/edit/main/website/src/content'
};
