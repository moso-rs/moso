<script lang="ts">
	let { command = 'cargo install moso-cli' }: { command?: string } = $props();

	let state = $state<'idle' | 'copied' | 'failed'>('idle');
	let timer: ReturnType<typeof setTimeout> | undefined;

	async function copy() {
		try {
			await navigator.clipboard.writeText(command);
			state = 'copied';
		} catch {
			state = 'failed';
		}
		clearTimeout(timer);
		timer = setTimeout(() => (state = 'idle'), 1600);
	}
</script>

<button class="install" type="button" onclick={copy} aria-label="Copy install command: {command}">
	<span class="install-prompt" aria-hidden="true">$</span>
	<code class="install-command">{command}</code>
	<span class="install-status" aria-live="polite">
		{#if state === 'copied'}
			Copied
		{:else if state === 'failed'}
			Copy failed
		{:else}
			Copy
		{/if}
	</span>
</button>
