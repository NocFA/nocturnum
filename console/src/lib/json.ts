// JSON pretty-printed into lines of coloured tokens, rendered by JsonView without {@html}.

export type Tok = { t: 'key' | 'str' | 'num' | 'bool' | 'null' | 'p' | 'ws' | 'note'; v: string };
export type Line = Tok[];

const MAX_ARRAY_INLINE = 6;

/** Whether a parsed value holds a number JSON.parse could not keep exactly: an integer beyond
 * 2^53, which it rounded, or one too large for a float, which became Infinity. */
export function hasUnsafeIntegers(v: unknown): boolean {
	if (typeof v === 'number') return !Number.isFinite(v) || (Number.isInteger(v) && !Number.isSafeInteger(v));
	if (Array.isArray(v)) return v.some(hasUnsafeIntegers);
	if (v !== null && typeof v === 'object') return Object.values(v).some(hasUnsafeIntegers);
	return false;
}

function scalar(v: unknown): Tok {
	if (v === null) return { t: 'null', v: 'null' };
	if (typeof v === 'boolean') return { t: 'bool', v: String(v) };
	if (typeof v === 'number') return { t: 'num', v: String(v) };
	return { t: 'str', v: JSON.stringify(v) };
}

function isScalar(v: unknown): boolean {
	return v === null || typeof v !== 'object';
}

/** Lines for `value`, with short arrays of scalars kept on one line. */
export function lines(value: unknown): Line[] {
	const out: Line[] = [];
	walk(value, 0, [], false, out);
	return out;
}

function walk(v: unknown, depth: number, prefix: Tok[], comma: boolean, out: Line[]) {
	const pad: Tok = { t: 'ws', v: '  '.repeat(depth) };
	const end: Tok[] = comma ? [{ t: 'p', v: ',' }] : [];
	if (Array.isArray(v)) {
		if (v.length === 0) {
			out.push([pad, ...prefix, { t: 'p', v: '[]' }, ...end]);
			return;
		}
		if (v.length <= MAX_ARRAY_INLINE && v.every(isScalar)) {
			const toks: Tok[] = [pad, ...prefix, { t: 'p', v: '[' }];
			v.forEach((x, i) => {
				toks.push(scalar(x));
				if (i < v.length - 1) toks.push({ t: 'p', v: ', ' });
			});
			toks.push({ t: 'p', v: ']' }, ...end);
			out.push(toks);
			return;
		}
		out.push([pad, ...prefix, { t: 'p', v: '[' }]);
		v.forEach((x, i) => walk(x, depth + 1, [], i < v.length - 1, out));
		out.push([pad, { t: 'p', v: ']' }, ...end]);
		return;
	}
	if (v !== null && typeof v === 'object') {
		const keys = Object.keys(v as object);
		if (keys.length === 0) {
			out.push([pad, ...prefix, { t: 'p', v: '{}' }, ...end]);
			return;
		}
		out.push([pad, ...prefix, { t: 'p', v: '{' }]);
		keys.forEach((k, i) =>
			walk(
				(v as Record<string, unknown>)[k],
				depth + 1,
				[{ t: 'key', v: JSON.stringify(k) }, { t: 'p', v: ': ' }],
				i < keys.length - 1,
				out
			)
		);
		out.push([pad, { t: 'p', v: '}' }, ...end]);
		return;
	}
	out.push([pad, ...prefix, scalar(v), ...end]);
}

/** A vector shown as a few rows of numbers and a note saying how many more there are. */
export function vectorLines(v: number[], shown = 16): Line[] {
	const out: Line[] = [[{ t: 'p', v: '[' }]];
	const head = v.slice(0, shown);
	for (let i = 0; i < head.length; i += 4) {
		const row: Tok[] = [{ t: 'ws', v: '  ' }];
		head.slice(i, i + 4).forEach((x, j, arr) => {
			row.push({ t: 'num', v: x.toFixed(4).padStart(7) });
			if (j < arr.length - 1 || i + 4 < v.length) row.push({ t: 'p', v: ', ' });
		});
		out.push(row);
	}
	if (v.length > shown) out.push([{ t: 'note', v: `  // ${v.length - shown} more of ${v.length}` }]);
	out.push([{ t: 'p', v: ']' }]);
	return out;
}
