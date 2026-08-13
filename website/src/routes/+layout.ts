/**
 * Every route is prerendered. The build emits a folder of HTML with no server
 * behind it, which is the whole point of the site: documentation should outlive
 * whatever infrastructure was fashionable when it was written.
 */
export const prerender = true;
export const trailingSlash = 'always';
