<script lang="ts">
	import { page } from '$app/state';
	import { PRIMARY_NAV, SITE } from '$lib/nav';
	import { href, isActive } from '$lib/links';
	import ThemeToggle from './ThemeToggle.svelte';
	import Search from './Search.svelte';
	import Logo from './Logo.svelte';

	let open = $state(false);

	// Any navigation closes the drawer. Leaving it open across a page change
	// leaves the reader on the destination page with the menu covering it.
	$effect(() => {
		void page.url.pathname;
		open = false;
	});
</script>

<header class="topbar">
	<div class="topbar-inner">
		<a class="brand" href={href('/')} aria-label="{SITE.name} home">
			<Logo />
			<span class="brand-name">{SITE.name}</span>
		</a>

		<nav class="topbar-nav" aria-label="Primary">
			{#each PRIMARY_NAV as item}
				<a
					href={href(item.href)}
					class="topbar-link"
					class:is-active={isActive(page.url.pathname, href(item.href))}
					aria-current={isActive(page.url.pathname, href(item.href)) ? 'page' : undefined}
				>
					{item.label}
				</a>
			{/each}
		</nav>

		<div class="topbar-actions">
			<Search />
			<a class="icon-link" href={SITE.repo} rel="noopener noreferrer" aria-label="Moso on GitHub">
				<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true" fill="currentColor">
					<path
						d="M12 .5a12 12 0 0 0-3.79 23.4c.6.1.82-.26.82-.58v-2c-3.34.73-4.04-1.6-4.04-1.6-.55-1.4-1.34-1.77-1.34-1.77-1.09-.75.08-.73.08-.73 1.2.08 1.84 1.24 1.84 1.24 1.07 1.84 2.8 1.3 3.49 1 .1-.78.42-1.31.76-1.61-2.67-.3-5.47-1.34-5.47-5.96 0-1.32.47-2.4 1.24-3.24-.13-.3-.54-1.52.11-3.18 0 0 1.01-.32 3.3 1.24a11.5 11.5 0 0 1 6.01 0c2.29-1.56 3.3-1.24 3.3-1.24.65 1.66.24 2.88.12 3.18.77.84 1.23 1.92 1.23 3.24 0 4.63-2.8 5.65-5.48 5.95.43.37.81 1.1.81 2.22v3.29c0 .32.22.69.83.57A12 12 0 0 0 12 .5Z"
					/>
				</svg>
			</a>
			<ThemeToggle />
			<button
				class="menu-button"
				type="button"
				aria-expanded={open}
				aria-controls="mobile-nav"
				onclick={() => (open = !open)}
			>
				<span class="visually-hidden">{open ? 'Close menu' : 'Open menu'}</span>
				<svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
					{#if open}
						<path d="M6 6l12 12M18 6 6 18" />
					{:else}
						<path d="M4 7h16M4 12h16M4 17h16" />
					{/if}
				</svg>
			</button>
		</div>
	</div>

	{#if open}
		<nav class="mobile-nav" id="mobile-nav" aria-label="Primary, mobile">
			{#each PRIMARY_NAV as item}
				<a href={href(item.href)} class="mobile-nav-link">{item.label}</a>
			{/each}
			<a href={SITE.repo} class="mobile-nav-link" rel="noopener noreferrer">GitHub</a>
		</nav>
	{/if}
</header>
