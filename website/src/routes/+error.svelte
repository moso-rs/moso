<script lang="ts">
	import { page } from '$app/state';
	import { href } from '$lib/links';
	import { navigation } from '$lib/content';
</script>

<svelte:head>
	<title>{page.status === 404 ? 'Page not found' : 'Something went wrong'} | Moso</title>
</svelte:head>

<div class="error-page">
	<p class="error-code">{page.status}</p>
	<h1>{page.status === 404 ? 'That page does not exist' : 'Something went wrong'}</h1>
	<p class="error-message">
		{#if page.status === 404}
			The documentation moves as the framework does. The page you wanted may have been renamed,
			or the link that brought you here may be out of date.
		{:else}
			{page.error?.message ?? 'An unexpected error occurred.'}
		{/if}
	</p>

	<div class="error-links">
		{#each navigation as section (section.slug)}
			<a href={href(`/docs/${section.slug}`)}>{section.label}</a>
		{/each}
	</div>
</div>
