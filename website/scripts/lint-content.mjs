#!/usr/bin/env node
/**
 * Content gate.
 *
 * Six rules, all of which have bitten this site at least once:
 *
 *  1. No em dashes or standalone en dashes. This is a house style rule and it
 *     is enforced rather than remembered.
 *  2. Every page has a title and a description, because both are load bearing:
 *     they build the sidebar, the search result, the meta tags and llms.txt.
 *  3. Descriptions are one sentence and fit in a search result.
 *  4. Every internal link resolves to a page that exists.
 *  5. Every fenced code block declares a language, so nothing falls back to
 *     unhighlighted grey.
 *  6. Every page starts its body with prose, not with a heading, because the
 *     template already renders the title as the `h1`.
 */

import { readdir, readFile } from 'node:fs/promises';
import { join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse as parseYaml } from 'yaml';

const ROOT = resolve(fileURLToPath(new URL('..', import.meta.url)));
const CONTENT = join(ROOT, 'src/content/docs');

const problems = [];
const pages = new Map();

function fail(file, line, message) {
	problems.push({ file, line, message });
}

async function walk(dir) {
	const out = [];
	for (const entry of await readdir(dir, { withFileTypes: true })) {
		const full = join(dir, entry.name);
		if (entry.isDirectory()) out.push(...(await walk(full)));
		else if (entry.name.endsWith('.md')) out.push(full);
	}
	return out;
}

function splitFrontmatter(source) {
	const match = source.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
	if (!match) return { data: null, body: source, offset: 0, error: 'missing frontmatter block' };

	const offset = match[0].split('\n').length - 1;
	try {
		return { data: parseYaml(match[1]) ?? {}, body: source.slice(match[0].length), offset };
	} catch (cause) {
		// An unquoted value containing a colon is by far the most common cause,
		// so say that rather than passing YAML's own message through alone.
		return {
			data: null,
			body: source.slice(match[0].length),
			offset,
			error: `invalid YAML frontmatter (${cause.message.split('\n')[0]}). Quote any value containing a colon.`
		};
	}
}

function urlFor(file) {
	const rel = relative(CONTENT, file).replace(/\\/g, '/');
	const slug = rel.replace(/\.md$/, '').replace(/\/index$/, '');
	return slug ? `/docs/${slug}` : '/docs';
}

/**
 * The directory a relative link in this file resolves against. For an
 * `index.md` that is its own directory, not its parent, which is why this
 * cannot be derived from `urlFor`.
 */
function dirFor(file) {
	const rel = relative(CONTENT, file).replace(/\\/g, '/');
	const at = rel.lastIndexOf('/');
	return at === -1 ? '/docs' : `/docs/${rel.slice(0, at)}`;
}

const files = await walk(CONTENT).catch(() => []);

if (files.length === 0) {
	console.error(`No content found under ${relative(ROOT, CONTENT)}`);
	process.exit(1);
}

// Pass one: collect every page so link checking has something to check against.
const parsed = [];
for (const file of files) {
	const source = await readFile(file, 'utf8');
	const { data, body, offset, error } = splitFrontmatter(source);
	pages.set(urlFor(file), data);
	parsed.push({ file, source, data, body, offset, error });
}

const KNOWN_EXTERNAL_PREFIXES = ['http://', 'https://', 'mailto:', '#'];

for (const { file, source, data, body, offset, error } of parsed) {
	const name = relative(ROOT, file);

	// Rule 1. Applies to the whole file, frontmatter included.
	source.split('\n').forEach((line, i) => {
		if (line.includes('—')) fail(name, i + 1, 'em dash (U+2014) is not allowed');
		// An en dash between spaces is being used as punctuation. Between digits
		// it is a range, which is fine.
		if (/\s–\s/.test(line)) fail(name, i + 1, 'en dash used as punctuation is not allowed');
	});

	if (!data) {
		fail(name, 1, error ?? 'missing frontmatter block');
		continue;
	}

	// Rules 2 and 3.
	if (!data.title) fail(name, 1, 'frontmatter has no `title`');
	if (!data.description) fail(name, 1, 'frontmatter has no `description`');
	if (typeof data.description === 'string') {
		if (data.description.length > 200) {
			fail(name, 1, `description is ${data.description.length} characters, keep it under 200`);
		}
		if (!/[.!?]$/.test(data.description.trim())) {
			fail(name, 1, 'description should end with a full stop');
		}
	}
	if (data.status && !['shipped', 'partial', 'planned'].includes(data.status)) {
		fail(name, 1, `unknown status \`${data.status}\``);
	}

	const lines = body.split('\n');

	// Rule 6.
	const firstContent = lines.find((l) => l.trim().length > 0);
	if (firstContent && firstContent.startsWith('# ')) {
		fail(name, offset + 1, 'body starts with an `h1`; the template renders the title already');
	}

	// Rule 5, plus link checking outside code blocks.
	let inFence = false;
	lines.forEach((line, i) => {
		const lineNo = offset + i + 1;
		const fence = line.match(/^\s*```(.*)$/);
		if (fence) {
			if (!inFence) {
				const info = fence[1].trim();
				if (info.length === 0) fail(name, lineNo, 'fenced code block has no language');
				inFence = true;
			} else {
				inFence = false;
			}
			return;
		}
		if (inFence) return;

		// Rule 4.
		for (const match of line.matchAll(/\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g)) {
			const target = match[1];
			if (KNOWN_EXTERNAL_PREFIXES.some((p) => target.startsWith(p))) continue;

			const [path] = target.split('#');
			if (!path) continue;

			let resolved;
			if (path.startsWith('/')) {
				resolved = path;
			} else {
				resolved = normalise(`${dirFor(file)}/${path}`);
			}
			resolved = resolved.replace(/(?:\/index)?\.md$/, '').replace(/\/$/, '');

			if (!pages.has(resolved) && resolved !== '/docs' && resolved !== '') {
				fail(name, lineNo, `link \`${target}\` resolves to \`${resolved}\`, which is not a page`);
			}
		}
	});

	if (inFence) fail(name, offset + lines.length, 'unclosed code fence');
}

function normalise(path) {
	const out = [];
	for (const part of path.split('/')) {
		if (part === '' || part === '.') continue;
		if (part === '..') out.pop();
		else out.push(part);
	}
	return `/${out.join('/')}`;
}

if (problems.length === 0) {
	console.log(`Content OK: ${files.length} pages checked.`);
	process.exit(0);
}

for (const p of problems) {
	console.error(`${p.file}:${p.line}  ${p.message}`);
}
console.error(`\n${problems.length} problem${problems.length === 1 ? '' : 's'} in ${files.length} pages.`);
process.exit(1);
