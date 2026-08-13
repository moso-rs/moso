import { base } from '$app/paths';

/**
 * Build an href for an internal path.
 *
 * The site is configured with `trailingSlash: 'always'`, so every internal link
 * has to carry the trailing slash itself. Getting this wrong does not break the
 * page, it just costs a redirect on hosts that issue one and a 404 on hosts
 * that do not, so it is centralised here rather than spelled out at each call
 * site.
 */
export function href(path: string): string {
	if (path === '/') return base || '/';
	const clean = path.startsWith('/') ? path : `/${path}`;
	return `${base}${clean.replace(/\/$/, '')}/`;
}

/** True when `current` is `path` or a page below it. */
export function isActive(current: string, path: string): boolean {
	const a = current.replace(/\/$/, '');
	const b = path.replace(/\/$/, '');
	return a === b || a.startsWith(`${b}/`);
}
