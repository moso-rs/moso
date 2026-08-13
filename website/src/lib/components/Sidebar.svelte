<script lang="ts">
	import { page } from '$app/state';
	import { navigation } from '$lib/content';
	import { href, isActive } from '$lib/links';

	let { id = 'docs-sidebar' }: { id?: string } = $props();

	const current = $derived(page.url.pathname);

	function sectionIsOpen(slug: string): boolean {
		return isActive(current, href(`/docs/${slug}`));
	}
</script>

<nav class="sidebar" {id} aria-label="Documentation">
	{#each navigation as section (section.slug)}
		{@const open = sectionIsOpen(section.slug)}
		<details class="sidebar-group" {open}>
			<summary class="sidebar-group-title">
				<svg class="sidebar-chevron" viewBox="0 0 24 24" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
					<path d="m9 6 6 6-6 6" />
				</svg>
				{section.label}
			</summary>

			<ul class="sidebar-list">
				{#if section.index}
					<li>
						<a
							class="sidebar-link"
							class:is-active={current === href(section.index.url)}
							href={href(section.index.url)}
							aria-current={current === href(section.index.url) ? 'page' : undefined}
						>
							Overview
						</a>
					</li>
				{/if}
				{#each section.pages as doc (doc.url)}
					<li>
						<a
							class="sidebar-link"
							class:is-active={current === href(doc.url)}
							href={href(doc.url)}
							aria-current={current === href(doc.url) ? 'page' : undefined}
						>
							{doc.metadata.sidebar ?? doc.metadata.title}
							{#if doc.metadata.status && doc.metadata.status !== 'shipped'}
								<span class="sidebar-tag sidebar-tag-{doc.metadata.status}">
									{doc.metadata.status === 'planned' ? 'planned' : 'partial'}
								</span>
							{/if}
						</a>
					</li>
				{/each}
			</ul>
		</details>
	{/each}
</nav>
