import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';
import { markdown } from './markdown/plugin.js';

export default defineConfig({
	plugins: [markdown(), sveltekit()],
	server: {
		fs: {
			allow: ['..']
		}
	},
	test: {
		include: ['tests/**/*.test.ts'],
		environment: 'node'
	}
});
