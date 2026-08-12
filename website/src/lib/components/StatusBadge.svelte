<script lang="ts">
	/**
	 * Honesty about what exists.
	 *
	 * Moso's design documents describe more than the workspace currently builds,
	 * so a page that documents something unfinished says so at the top rather
	 * than letting a reader discover it from a compile error. The wording is
	 * deliberately blunt.
	 */
	let { status }: { status?: 'shipped' | 'partial' | 'planned' } = $props();

	const copy = {
		shipped: { label: 'Shipped', detail: 'Implemented, tested, and described accurately here.' },
		partial: {
			label: 'Partial',
			detail: 'The core works. Some of what this page describes is not built yet, and is marked inline.'
		},
		planned: {
			label: 'Planned',
			detail: 'Design intent. There is no implementation in the workspace yet.'
		}
	} as const;
</script>

{#if status && status !== 'shipped'}
	<p class="status-badge status-{status}">
		<strong>{copy[status].label}.</strong>
		{copy[status].detail}
	</p>
{/if}
