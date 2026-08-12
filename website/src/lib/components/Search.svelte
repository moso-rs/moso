<script lang="ts">
	/**
	 * Full text search over the built site.
	 *
	 * Pagefind indexes the generated HTML after the build and ships a set of
	 * chunked indexes that are fetched on demand, so the search payload scales
	 * with what the reader actually types rather than with the size of the site.
	 * Nothing is loaded until the dialog is opened for the first time.
	 *
	 * In `vite dev` there is no index yet, because there is no build output to
	 * index. Rather than fail silently the dialog says so and offers the section
	 * list instead.
	 */
	import { onMount, tick } from 'svelte';
	import { base } from '$app/paths';

	interface PagefindResult {
		id: string;
		data: () => Promise<{
			url: string;
			excerpt: string;
			meta: { title?: string };
			raw_url?: string;
		}>;
	}

	interface PagefindApi {
		init?: () => Promise<void>;
		options?: (o: Record<string, unknown>) => Promise<void>;
		search: (q: string) => Promise<{ results: PagefindResult[] }>;
	}

	interface Hit {
		url: string;
		title: string;
		excerpt: string;
	}

	let dialog: HTMLDialogElement;
	let input = $state<HTMLInputElement | null>(null);
	let query = $state('');
	let hits = $state<Hit[]>([]);
	let selected = $state(0);
	let status = $state<'idle' | 'loading' | 'ready' | 'unavailable'>('idle');
	let pagefind: PagefindApi | null = null;
	let token = 0;

	async function load() {
		if (pagefind || status === 'unavailable') return;
		status = 'loading';
		try {
			const url = `${base}/_pagefind/pagefind.js`;
			pagefind = (await import(/* @vite-ignore */ url)) as PagefindApi;
			await pagefind.options?.({ excerptLength: 22 });
			await pagefind.init?.();
			status = 'ready';
		} catch {
			status = 'unavailable';
		}
	}

	async function open() {
		dialog?.showModal();
		await tick();
		input?.focus();
		void load();
	}

	function close() {
		dialog?.close();
	}

	async function run(q: string) {
		if (!pagefind || q.trim().length < 2) {
			hits = [];
			return;
		}
		const mine = ++token;
		const search = await pagefind.search(q);
		if (mine !== token) return;

		const top = await Promise.all(search.results.slice(0, 12).map((r) => r.data()));
		if (mine !== token) return;

		hits = top.map((d) => ({
			url: d.url.replace(/\.html$/, '').replace(/\/index$/, '/') || '/',
			title: d.meta?.title ?? 'Untitled',
			excerpt: d.excerpt
		}));
		selected = 0;
	}

	$effect(() => {
		const q = query;
		if (status === 'ready') void run(q);
	});

	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown') {
			event.preventDefault();
			selected = Math.min(selected + 1, hits.length - 1);
		} else if (event.key === 'ArrowUp') {
			event.preventDefault();
			selected = Math.max(selected - 1, 0);
		} else if (event.key === 'Enter' && hits[selected]) {
			event.preventDefault();
			window.location.href = hits[selected].url;
		}
	}

	onMount(() => {
		function shortcut(event: KeyboardEvent) {
			if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
				event.preventDefault();
				if (dialog?.open) close();
				else void open();
			}
		}
		window.addEventListener('keydown', shortcut);
		return () => window.removeEventListener('keydown', shortcut);
	});
</script>

<button class="search-trigger" type="button" onclick={open} aria-label="Search the documentation">
	<svg viewBox="0 0 24 24" width="15" height="15" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2">
		<circle cx="11" cy="11" r="7" />
		<path d="m20 20-3.6-3.6" stroke-linecap="round" />
	</svg>
	<span class="search-trigger-label">Search</span>
	<kbd class="search-trigger-key"><span aria-hidden="true">⌘</span>K</kbd>
</button>

<dialog
	class="search-dialog"
	bind:this={dialog}
	onclose={() => {
		query = '';
		hits = [];
	}}
	onclick={(e) => {
		if (e.target === dialog) close();
	}}
	aria-label="Search"
>
	<div class="search-panel">
		<div class="search-field">
			<svg viewBox="0 0 24 24" width="17" height="17" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2">
				<circle cx="11" cy="11" r="7" />
				<path d="m20 20-3.6-3.6" stroke-linecap="round" />
			</svg>
			<input
				bind:this={input}
				bind:value={query}
				onkeydown={onKeydown}
				type="search"
				placeholder="Search the documentation"
				aria-label="Search query"
				autocomplete="off"
				spellcheck="false"
			/>
			<button class="search-close" type="button" onclick={close} aria-label="Close search">Esc</button>
		</div>

		<div class="search-results" role="listbox" aria-label="Search results">
			{#if status === 'unavailable'}
				<p class="search-note">
					Search runs against the built site. Start it with <code>pnpm build</code> and
					<code>pnpm preview</code>, or browse the sections from the navigation.
				</p>
			{:else if status === 'loading'}
				<p class="search-note">Loading the index.</p>
			{:else if query.trim().length < 2}
				<p class="search-note">Type at least two characters.</p>
			{:else if hits.length === 0}
				<p class="search-note">No matches for <strong>{query}</strong>.</p>
			{:else}
				<ul>
					{#each hits as hit, i}
						<li>
							<a
								class="search-hit"
								class:is-selected={i === selected}
								href={hit.url}
								role="option"
								aria-selected={i === selected}
								onmouseenter={() => (selected = i)}
							>
								<span class="search-hit-title">{hit.title}</span>
								<!-- Pagefind returns its own highlighted excerpt markup. -->
								<!-- eslint-disable-next-line svelte/no-at-html-tags -->
								<span class="search-hit-excerpt">{@html hit.excerpt}</span>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	</div>
</dialog>
