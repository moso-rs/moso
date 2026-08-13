<script lang="ts">
	import Prose from '$lib/components/Prose.svelte';
	import Toc from '$lib/components/Toc.svelte';
	import PrevNext from '$lib/components/PrevNext.svelte';
	import StatusBadge from '$lib/components/StatusBadge.svelte';
	import { SITE } from '$lib/nav';
	import { href } from '$lib/links';
	import { docByUrl } from '$lib/content';

	let { data } = $props();

	const doc = $derived(data.doc);
	const related = $derived(
		(doc.metadata.related ?? []).map((url) => docByUrl.get(url.replace(/\/$/, ''))).filter(Boolean)
	);
	const editUrl = $derived(
		`${SITE.editBase}/docs/${doc.slug}${doc.slug === doc.section ? '/index.md' : '.md'}`
	);
</script>

<svelte:head>
	<title>{doc.metadata.title} | {SITE.name}</title>
	<meta name="description" content={doc.metadata.description} />
	<meta property="og:title" content="{doc.metadata.title} | {SITE.name}" />
	<meta property="og:description" content={doc.metadata.description} />
	<meta property="og:type" content="article" />
	<meta name="twitter:card" content="summary" />
	<link rel="canonical" href="{SITE.url}{doc.url}/" />
</svelte:head>

<article class="docs-article" data-pagefind-body>
	<header class="docs-header">
		{#if data.section}
			<p class="docs-eyebrow">
				<a href={href(`/docs/${data.section.slug}`)}>{data.section.label}</a>
			</p>
		{/if}
		<h1 data-pagefind-meta="title">{doc.metadata.title}</h1>
		<p class="docs-lead">{doc.metadata.description}</p>
		<StatusBadge status={doc.metadata.status} />
	</header>

	<Prose html={doc.html} />

	{#if related.length > 0}
		<aside class="related" aria-label="Related pages">
			<h2>Related</h2>
			<ul>
				{#each related as page}
					<li>
						<a href={href(page.url)}>
							<strong>{page.metadata.title}</strong>
							<span>{page.metadata.description}</span>
						</a>
					</li>
				{/each}
			</ul>
		</aside>
	{/if}

	<footer class="docs-footer">
		<a class="edit-link" href={editUrl} rel="noopener noreferrer">Edit this page on GitHub</a>
	</footer>

	<PrevNext prev={data.prev} next={data.next} />
</article>

<div class="docs-aside">
	<Toc headings={doc.headings} />
</div>
