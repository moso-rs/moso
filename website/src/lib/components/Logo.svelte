<script lang="ts">
	/**
	 * The mark is a bamboo culm read as a stack: three segments separated by two
	 * nodes, which is also the framework's layer cake (substrate, runtime,
	 * batteries). It is drawn rather than imported so it inherits `currentColor`
	 * and needs no network request in either theme.
	 */
	let { size = 22 }: { size?: number } = $props();
</script>

<svg
	class="logo"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	role="img"
	aria-hidden="true"
	fill="none"
>
	<rect x="6.5" y="2.5" width="11" height="19" rx="3.2" stroke="currentColor" stroke-width="1.9" />
	<path d="M6.5 9.2h11M6.5 14.8h11" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" />
	<path d="M12 9.2v5.6" stroke="var(--accent)" stroke-width="1.9" stroke-linecap="round" />
</svg>
