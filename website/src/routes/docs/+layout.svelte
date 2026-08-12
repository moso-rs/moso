<script lang="ts">
	import Sidebar from '$lib/components/Sidebar.svelte';

	let { children } = $props();
	let navOpen = $state(false);
</script>

<div class="docs-shell">
	<button
		class="sidebar-toggle"
		type="button"
		aria-expanded={navOpen}
		aria-controls="docs-sidebar"
		onclick={() => (navOpen = !navOpen)}
	>
		<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
			<path d="M4 7h16M4 12h16M4 17h16" />
		</svg>
		Documentation menu
	</button>

	<aside class="docs-sidebar" class:is-open={navOpen}>
		<Sidebar />
	</aside>

	{@render children()}
</div>
