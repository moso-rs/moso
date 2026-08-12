<script lang="ts">
	import '../styles/tokens.css';
	import '../styles/app.css';
	import '../styles/prose.css';
	import Nav from '$lib/components/Nav.svelte';
	import Footer from '$lib/components/Footer.svelte';

	let { children } = $props();
</script>

<a class="skip-link" href="#main">Skip to content</a>

<div class="site">
	<Nav />
	<main id="main" tabindex="-1">
		{@render children()}
	</main>
	<Footer />
</div>
