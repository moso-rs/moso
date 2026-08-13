<script lang="ts">
	import type { Heading } from '$lib/types';

	let { headings }: { headings: Heading[] } = $props();

	let active = $state<string>('');

	function spy(node: HTMLElement) {
		if (headings.length === 0) return;

		// Track which headings are currently above the reading line rather than
		// which are merely visible. Plain visibility marks the last heading of a
		// long section as inactive as soon as its text scrolls off, which makes
		// the highlight jump around while the reader has not moved sections.
		const targets = headings
			.map((h) => document.getElementById(h.id))
			.filter((el): el is HTMLElement => el !== null);

		if (targets.length === 0) return;

		let ticking = false;

		function update() {
			ticking = false;
			const line = window.scrollY + 120;
			let current = targets[0]?.id ?? '';
			for (const el of targets) {
				if (el.offsetTop <= line) current = el.id;
				else break;
			}
			// At the very bottom the last heading wins, otherwise a short final
			// section can never become active.
			if (window.innerHeight + window.scrollY >= document.body.offsetHeight - 24) {
				current = targets[targets.length - 1].id;
			}
			active = current;
		}

		function onScroll() {
			if (ticking) return;
			ticking = true;
			requestAnimationFrame(update);
		}

		update();
		window.addEventListener('scroll', onScroll, { passive: true });
		window.addEventListener('resize', onScroll, { passive: true });

		return {
			destroy() {
				window.removeEventListener('scroll', onScroll);
				window.removeEventListener('resize', onScroll);
			}
		};
	}
</script>

{#if headings.length > 1}
	<nav class="toc" aria-label="On this page" use:spy>
		<h2 class="toc-title">On this page</h2>
		<ul class="toc-list">
			{#each headings as heading}
				<li class="toc-item toc-depth-{heading.depth}">
					<a
						href="#{heading.id}"
						class="toc-link"
						class:is-active={active === heading.id}
						aria-current={active === heading.id ? 'true' : undefined}
					>
						{heading.text}
					</a>
				</li>
			{/each}
		</ul>
	</nav>
{/if}
