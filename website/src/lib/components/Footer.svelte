<script lang="ts">
	import { SITE } from '$lib/nav';
	import { href } from '$lib/links';
	import Logo from './Logo.svelte';

	const columns = [
		{
			title: 'Learn',
			links: [
				{ label: 'Introduction', href: '/docs/start' },
				{ label: 'Installation', href: '/docs/start/installation' },
				{ label: 'Quick start', href: '/docs/start/quick-start' },
				{ label: 'Guides', href: '/docs/guides' }
			]
		},
		{
			title: 'Build',
			links: [
				{ label: 'Routing', href: '/docs/guides/routing' },
				{ label: 'Schemas', href: '/docs/guides/schemas' },
				{ label: 'OpenAPI', href: '/docs/guides/openapi' },
				{ label: 'Testing', href: '/docs/guides/testing' }
			]
		},
		{
			title: 'Operate',
			links: [
				{ label: 'Configuration', href: '/docs/guides/configuration' },
				{ label: 'Security', href: '/docs/guides/security' },
				{ label: 'Observability', href: '/docs/guides/observability' },
				{ label: 'Health and shutdown', href: '/docs/guides/health-and-shutdown' }
			]
		}
	];
</script>

<footer class="footer">
	<div class="footer-inner">
		<div class="footer-brand">
			<a class="brand" href={href('/')} aria-label="{SITE.name} home">
				<Logo />
				<span class="brand-name">{SITE.name}</span>
			</a>
			<p class="footer-tagline">{SITE.tagline}.</p>
			<p class="footer-licence">
				Licensed under <a href="{SITE.repo}/blob/main/LICENSE" rel="noopener noreferrer">MIT</a>, permanently.
			</p>
		</div>

		{#each columns as column}
			<nav class="footer-column" aria-label={column.title}>
				<h2 class="footer-heading">{column.title}</h2>
				<ul>
					{#each column.links as link}
						<li><a href={href(link.href)}>{link.label}</a></li>
					{/each}
				</ul>
			</nav>
		{/each}

		<nav class="footer-column" aria-label="Project">
			<h2 class="footer-heading">Project</h2>
			<ul>
				<li><a href={SITE.repo} rel="noopener noreferrer">GitHub</a></li>
				<li><a href="{SITE.repo}/blob/main/AGENTS.md" rel="noopener noreferrer">Contributing</a></li>
				<li><a href="{SITE.repo}/security/advisories/new" rel="noopener noreferrer">Security</a></li>
			</ul>
		</nav>
	</div>
</footer>
