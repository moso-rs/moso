/**
 * Syntax themes for code blocks.
 *
 * These are TextMate themes handed to Shiki at build time. They are written by
 * hand rather than pulled from Shiki's bundle so the code palette belongs to
 * the same design system as the rest of the site, and so both themes assign the
 * same hue to the same meaning: a keyword is the same colour family in light
 * and dark, only its lightness moves.
 *
 * Rust drives the choices, because Rust is what the reader is here for. The
 * scopes below are the ones Rust actually uses: `storage.type` for `let`,
 * `fn` and `struct`, `entity.name.type` for type names, `support.function` for
 * macros, `meta.attribute` for `#[derive(...)]`, and `variable.other` for
 * bindings.
 *
 * Every foreground was checked against its own background for at least 4.5:1,
 * with comments (the lowest contrast token by design) at no less than 4.5:1 so
 * that commented code stays readable rather than becoming decoration.
 */

/** @typedef {{ scope: string[] | string, settings: { foreground?: string, fontStyle?: string } }} TokenColor */

const light = {
	bg: '#f7f8fa',
	fg: '#26292f',
	comment: '#67707e',
	keyword: '#8a3fa8',
	control: '#8a3fa8',
	string: '#1f7a4d',
	number: '#9a5510',
	type: '#0f6f8f',
	func: '#2f5fd0',
	macro: '#a03a6b',
	attribute: '#7a6410',
	constant: '#9a5510',
	punctuation: '#5b6472',
	variable: '#26292f',
	lifetime: '#a03a6b',
	invalid: '#b3261e'
};

const dark = {
	bg: '#12151a',
	fg: '#d3d9e3',
	comment: '#8892a4',
	keyword: '#d6a3ee',
	control: '#d6a3ee',
	string: '#8fd6a9',
	number: '#e8b17e',
	type: '#79ccdf',
	func: '#a8c2ff',
	macro: '#f0a5c4',
	attribute: '#d8c98a',
	constant: '#e8b17e',
	punctuation: '#98a2b3',
	variable: '#d3d9e3',
	lifetime: '#f0a5c4',
	invalid: '#ff9a92'
};

/**
 * @param {typeof light} p
 * @returns {TokenColor[]}
 */
function tokens(p) {
	return [
		{ scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: p.comment, fontStyle: 'italic' } },
		{
			scope: ['string', 'string.quoted', 'constant.character', 'markup.inline.raw', 'meta.attribute string'],
			settings: { foreground: p.string }
		},
		{ scope: ['constant.character.escape'], settings: { foreground: p.macro } },
		{
			scope: ['keyword', 'keyword.control', 'keyword.operator.expression', 'storage', 'storage.type', 'storage.modifier'],
			settings: { foreground: p.keyword }
		},
		{ scope: ['keyword.operator'], settings: { foreground: p.punctuation } },
		{
			scope: ['constant.language', 'constant.language.boolean', 'variable.language.self', 'variable.language.this'],
			settings: { foreground: p.control }
		},
		{ scope: ['constant.numeric', 'constant.other.rust'], settings: { foreground: p.number } },
		{
			scope: [
				'entity.name.type',
				'entity.name.namespace',
				'entity.name.class',
				'entity.name.struct',
				'entity.name.enum',
				'entity.name.trait',
				'support.type',
				'support.class',
				'storage.type.core',
				'meta.generic'
			],
			settings: { foreground: p.type }
		},
		{
			scope: ['entity.name.function', 'meta.function-call', 'support.function', 'variable.function'],
			settings: { foreground: p.func }
		},
		{
			scope: ['entity.name.function.macro', 'support.macro', 'meta.macro', 'entity.name.function.macro.rules'],
			settings: { foreground: p.macro }
		},
		{
			scope: ['meta.attribute', 'meta.attribute.rust', 'entity.name.attribute-name', 'storage.type.attribute'],
			settings: { foreground: p.attribute }
		},
		{
			scope: ['storage.modifier.lifetime', 'entity.name.lifetime', 'punctuation.definition.lifetime'],
			settings: { foreground: p.lifetime, fontStyle: 'italic' }
		},
		{
			scope: ['punctuation', 'meta.brace', 'punctuation.definition', 'punctuation.separator', 'punctuation.terminator'],
			settings: { foreground: p.punctuation }
		},
		{
			scope: ['variable', 'variable.other', 'meta.variable', 'variable.parameter'],
			settings: { foreground: p.variable }
		},
		{ scope: ['variable.other.member', 'meta.field', 'entity.name.variable.field'], settings: { foreground: p.fg } },
		{ scope: ['constant.other.caps', 'constant.other.rust'], settings: { foreground: p.constant } },
		// Structured formats.
		{ scope: ['support.type.property-name', 'meta.object-literal.key', 'entity.name.tag.yaml'], settings: { foreground: p.type } },
		{ scope: ['entity.name.tag'], settings: { foreground: p.keyword } },
		{ scope: ['entity.other.attribute-name'], settings: { foreground: p.attribute } },
		// Diffs, used in the migration and upgrade pages.
		{ scope: ['markup.inserted', 'meta.diff.header.to-file'], settings: { foreground: p.string } },
		{ scope: ['markup.deleted', 'meta.diff.header.from-file'], settings: { foreground: p.invalid } },
		{ scope: ['markup.changed'], settings: { foreground: p.number } },
		{ scope: ['invalid', 'invalid.illegal'], settings: { foreground: p.invalid } }
	];
}

/** @param {'light' | 'dark'} kind */
function theme(kind) {
	const p = kind === 'light' ? light : dark;
	return {
		name: `moso-${kind}`,
		type: kind,
		colors: {
			'editor.background': p.bg,
			'editor.foreground': p.fg
		},
		settings: [{ settings: { background: p.bg, foreground: p.fg } }, ...tokens(p)],
		tokenColors: tokens(p)
	};
}

export const SYNTAX_LIGHT = theme('light');
export const SYNTAX_DARK = theme('dark');
