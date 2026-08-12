import { describe, expect, it } from 'vitest';
import { readdir, readFile } from 'node:fs/promises';
import { join, relative, resolve } from 'node:path';
import { parse as parseYaml } from 'yaml';
import { SECTIONS } from '../src/lib/nav';

/**
 * Invariants over the content itself.
 *
 * These run against the files on disk rather than through the Vite content
 * layer, so they hold even when the app fails to build, and they say which file
 * is at fault rather than which import chain is.
 */

const ROOT = resolve(import.meta.dirname, '..');
const CONTENT = join(ROOT, 'src/content/docs');

interface Page {
	path: string;
	name: string;
	url: string;
	dir: string;
	section: string;
	data: Record<string, unknown>;
	body: string;
}

async function walk(dir: string): Promise<string[]> {
	const out: string[] = [];
	for (const entry of await readdir(dir, { withFileTypes: true })) {
		const full = join(dir, entry.name);
		if (entry.isDirectory()) out.push(...(await walk(full)));
		else if (entry.name.endsWith('.md')) out.push(full);
	}
	return out;
}

async function loadPages(): Promise<Page[]> {
	const files = await walk(CONTENT);
	return Promise.all(
		files.map(async (path) => {
			const source = await readFile(path, 'utf8');
			const match = source.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
			const data = match ? ((parseYaml(match[1]) ?? {}) as Record<string, unknown>) : {};
			const body = match ? source.slice(match[0].length) : source;
			const rel = relative(CONTENT, path).replace(/\\/g, '/');
			const slug = rel.replace(/\.md$/, '').replace(/\/index$/, '');
			const lastSlash = rel.lastIndexOf('/');
			return {
				path,
				name: relative(ROOT, path),
				url: slug ? `/docs/${slug}` : '/docs',
				// Relative links resolve against the source file's directory,
				// which for an index.md is its own directory rather than its
				// parent. Deriving this from `url` would be wrong for exactly
				// the pages that link the most.
				dir: lastSlash === -1 ? '/docs' : `/docs/${rel.slice(0, lastSlash)}`,
				section: slug.split('/')[0] ?? '',
				data,
				body
			};
		})
	);
}

const pages = await loadPages();
const urls = new Set(pages.map((p) => p.url));

describe('content set', () => {
	it('has pages', () => {
		expect(pages.length).toBeGreaterThan(0);
	});

	it('only uses sections declared in the navigation', () => {
		const known = new Set(SECTIONS.map((s) => s.slug));
		const unknown = pages.filter((p) => !known.has(p.section)).map((p) => p.name);
		expect(unknown).toEqual([]);
	});

	it('gives every section an index page', () => {
		const withPages = new Set(pages.map((p) => p.section));
		const missing = [...withPages].filter((s) => !urls.has(`/docs/${s}`));
		expect(missing).toEqual([]);
	});
});

describe('frontmatter', () => {
	it('every page has a title', () => {
		expect(pages.filter((p) => !p.data.title).map((p) => p.name)).toEqual([]);
	});

	it('every page has a description', () => {
		expect(pages.filter((p) => !p.data.description).map((p) => p.name)).toEqual([]);
	});

	it('descriptions are one short sentence', () => {
		const bad = pages
			.filter((p) => {
				const d = p.data.description;
				return typeof d !== 'string' || d.length > 200 || !/[.!?]$/.test(d.trim());
			})
			.map((p) => p.name);
		expect(bad).toEqual([]);
	});

	it('statuses are from the known set', () => {
		const bad = pages
			.filter((p) => p.data.status && !['shipped', 'partial', 'planned'].includes(String(p.data.status)))
			.map((p) => `${p.name}: ${p.data.status}`);
		expect(bad).toEqual([]);
	});

	it('orders are unique within a section', () => {
		const clashes: string[] = [];
		for (const section of SECTIONS) {
			const seen = new Map<number, string>();
			for (const page of pages.filter((p) => p.section === section.slug && p.url !== `/docs/${section.slug}`)) {
				const order = page.data.order;
				if (typeof order !== 'number') continue;
				const already = seen.get(order);
				if (already) clashes.push(`${section.slug}: ${already} and ${page.name} share order ${order}`);
				else seen.set(order, page.name);
			}
		}
		expect(clashes).toEqual([]);
	});
});

describe('house style', () => {
	it('contains no em dashes', () => {
		const offenders: string[] = [];
		for (const page of pages) {
			const source = `${JSON.stringify(page.data)}\n${page.body}`;
			source.split('\n').forEach((line, i) => {
				if (line.includes('—')) offenders.push(`${page.name}:${i + 1}`);
			});
		}
		expect(offenders).toEqual([]);
	});

	it('contains no en dashes used as punctuation', () => {
		const offenders: string[] = [];
		for (const page of pages) {
			page.body.split('\n').forEach((line, i) => {
				if (/\s–\s/.test(line)) offenders.push(`${page.name}:${i + 1}`);
			});
		}
		expect(offenders).toEqual([]);
	});

	it('never repeats the title as an h1 in the body', () => {
		const offenders = pages.filter((p) => /^#\s/m.test(p.body)).map((p) => p.name);
		expect(offenders).toEqual([]);
	});

	it('gives every code fence a language', () => {
		const offenders: string[] = [];
		for (const page of pages) {
			let open = false;
			page.body.split('\n').forEach((line, i) => {
				const fence = line.match(/^\s*```(.*)$/);
				if (!fence) return;
				if (!open && fence[1].trim() === '') offenders.push(`${page.name}:${i + 1}`);
				open = !open;
			});
			if (open) offenders.push(`${page.name}: unclosed fence`);
		}
		expect(offenders).toEqual([]);
	});
});

describe('links', () => {
	function normalise(path: string): string {
		const out: string[] = [];
		for (const part of path.split('/')) {
			if (part === '' || part === '.') continue;
			if (part === '..') out.pop();
			else out.push(part);
		}
		return `/${out.join('/')}`;
	}

	it('every internal link resolves to a page that exists', () => {
		const broken: string[] = [];

		for (const page of pages) {
			let inFence = false;
			page.body.split('\n').forEach((line, i) => {
				if (/^\s*```/.test(line)) {
					inFence = !inFence;
					return;
				}
				if (inFence) return;

				for (const match of line.matchAll(/\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g)) {
					const target = match[1];
					if (/^(https?:|mailto:|#)/.test(target)) continue;

					const [path] = target.split('#');
					if (!path) continue;

					const resolved = (path.startsWith('/') ? path : normalise(`${page.dir}/${path}`))
						.replace(/(?:\/index)?\.md$/, '')
						.replace(/\/$/, '');

					if (!urls.has(resolved)) broken.push(`${page.name}:${i + 1} -> ${target}`);
				}
			});
		}

		expect(broken).toEqual([]);
	});
});
