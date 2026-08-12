<script lang="ts">
	/**
	 * Renders compiled markdown and adds the two things that have to happen in
	 * the browser: a copy button on every code block, and a filename bar for the
	 * blocks whose fence carried a `title=`.
	 *
	 * Both are done here rather than in the markdown pipeline so that the HTML
	 * in the build output stays clean and indexable: search should find the code,
	 * not the word "Copy" repeated ninety times.
	 */
	let { html }: { html: string } = $props();

	function enhance(node: HTMLElement) {
		function decorate() {
			for (const pre of Array.from(node.querySelectorAll<HTMLPreElement>('pre'))) {
				if (pre.dataset.enhanced) continue;
				pre.dataset.enhanced = 'true';

				const frame = document.createElement('div');
				frame.className = 'code-frame';
				if (pre.hasAttribute('data-title')) frame.classList.add('has-title');

				pre.replaceWith(frame);
				frame.appendChild(pre);

				const title = pre.getAttribute('data-title');
				if (title) {
					const bar = document.createElement('div');
					bar.className = 'code-frame-bar';
					const name = document.createElement('span');
					name.className = 'code-frame-name';
					name.textContent = title;
					bar.appendChild(name);
					frame.insertBefore(bar, pre);
				}

				if (pre.hasAttribute('data-nocopy')) continue;

				const button = document.createElement('button');
				button.type = 'button';
				button.className = 'code-copy';
				button.textContent = 'Copy';
				button.setAttribute('aria-label', 'Copy code to clipboard');

				button.addEventListener('click', async () => {
					const code = pre.querySelector('code')?.textContent ?? '';
					try {
						await navigator.clipboard.writeText(code);
						button.textContent = 'Copied';
						button.classList.add('is-done');
					} catch {
						button.textContent = 'Press Ctrl C';
					}
					setTimeout(() => {
						button.textContent = 'Copy';
						button.classList.remove('is-done');
					}, 1600);
				});

				frame.appendChild(button);
			}
		}

		decorate();

		// The same component instance is reused across navigations, so redecorate
		// whenever the rendered markup is swapped out.
		const observer = new MutationObserver(decorate);
		observer.observe(node, { childList: true });

		return {
			destroy() {
				observer.disconnect();
			}
		};
	}
</script>

<div class="prose" use:enhance>
	<!-- Content is authored in this repository and compiled at build time. -->
	<!-- eslint-disable-next-line svelte/no-at-html-tags -->
	{@html html}
</div>
