<script lang="ts">
	/**
	 * Light and dark are both explicit choices; there is no "system" third state
	 * in the UI because a tri-state toggle is a puzzle, not a control. The
	 * initial value still follows the system preference, resolved by the inline
	 * script in `app.html` before first paint. Choosing here writes the choice
	 * down, and from then on it wins.
	 */
	import { onMount } from 'svelte';

	let theme = $state<'light' | 'dark'>('light');
	let mounted = $state(false);

	onMount(() => {
		theme = document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light';
		mounted = true;
	});

	function toggle() {
		theme = theme === 'dark' ? 'light' : 'dark';
		document.documentElement.dataset.theme = theme;
		try {
			localStorage.setItem('moso-theme', theme);
		} catch {
			// Private browsing modes can refuse storage. The toggle still works
			// for this page load, which is the part the reader asked for.
		}
	}
</script>

<button
	class="theme-toggle"
	type="button"
	onclick={toggle}
	aria-label={mounted ? `Switch to ${theme === 'dark' ? 'light' : 'dark'} theme` : 'Switch theme'}
	title="Switch theme"
>
	<svg class="icon-sun" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
		<circle cx="12" cy="12" r="4.2" fill="currentColor" />
		<g stroke="currentColor" stroke-width="1.7" stroke-linecap="round">
			<path d="M12 2.6v2.4M12 19v2.4M4.4 12H2M22 12h-2.4M6.3 6.3 4.6 4.6M19.4 19.4l-1.7-1.7M17.7 6.3l1.7-1.7M4.6 19.4l1.7-1.7" />
		</g>
	</svg>
	<svg class="icon-moon" viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
		<path
			d="M20.2 14.6A8.6 8.6 0 0 1 9.4 3.8a8.6 8.6 0 1 0 10.8 10.8Z"
			fill="currentColor"
		/>
	</svg>
</button>
