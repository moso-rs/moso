<script lang="ts">
	import type { Doc } from '$lib/types';
	import { href } from '$lib/links';

	let { prev, next }: { prev?: Doc; next?: Doc } = $props();
</script>

{#if prev || next}
	<nav class="prevnext" aria-label="Previous and next page">
		{#if prev}
			<a class="prevnext-link prevnext-prev" href={href(prev.url)} rel="prev">
				<span class="prevnext-label">Previous</span>
				<span class="prevnext-title">{prev.metadata.title}</span>
			</a>
		{:else}
			<span></span>
		{/if}
		{#if next}
			<a class="prevnext-link prevnext-next" href={href(next.url)} rel="next">
				<span class="prevnext-label">Next</span>
				<span class="prevnext-title">{next.metadata.title}</span>
			</a>
		{/if}
	</nav>
{/if}
