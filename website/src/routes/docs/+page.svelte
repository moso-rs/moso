<script lang="ts">
	import { navigation } from '$lib/content';
	import { href } from '$lib/links';
	import { SITE } from '$lib/nav';
</script>

<svelte:head>
	<title>Documentation | {SITE.name}</title>
	<meta name="description" content="Every part of the Moso framework, from a first endpoint to the design decisions behind the ORM." />
</svelte:head>

<article class="docs-article docs-index">
	<header class="docs-header">
		<h1>Documentation</h1>
		<p class="docs-lead">
			Moso is a batteries-included Rust web framework. Start with a running endpoint, then work
			outwards: the guides cover every feature the framework ships, with the code you would
			actually write.
		</p>
	</header>

	<div class="section-grid">
		{#each navigation as section (section.slug)}
			<a class="section-card" href={href(`/docs/${section.slug}`)}>
				<h2>{section.label}</h2>
				<p>{section.blurb}</p>
				<span class="section-card-count">
					{section.pages.length}
					{section.pages.length === 1 ? 'page' : 'pages'}
				</span>
			</a>
		{/each}
	</div>
</article>
