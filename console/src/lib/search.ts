// The top search, read as a filter for points tables.
//
//   41207                 the point with that id, a number or a UUID
//   source:mail           payload field equals a value, numbers and true/false as such
//   price>10  price<=20   a range on a number
//   group commit          words found anywhere in the payloads already loaded

import type { Json, Payload, PointId } from './api';

export interface Query {
	/** Sent to the server with scroll, `null` when the search has nothing for it. */
	filter: Json | null;
	/** Words matched against payload text in the browser. */
	words: string[];
}

const UUID = /^[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}$/i;

function value(raw: string): Json {
	if (raw === 'true') return true;
	if (raw === 'false') return false;
	if (/^-?\d+$/.test(raw) && Number.isSafeInteger(Number(raw))) return Number(raw);
	return raw.replace(/^"(.*)"$/, '$1');
}

export function parse(q: string): Query {
	const must: Json[] = [];
	const ids: PointId[] = [];
	const words: string[] = [];
	for (const part of q.trim().split(/\s+/).filter(Boolean)) {
		let m: RegExpMatchArray | null;
		if (/^\d+$/.test(part) || UUID.test(part)) {
			ids.push(/^\d+$/.test(part) ? part : part.toLowerCase());
		} else if ((m = part.match(/^([^:<>=\s]+)(<=|>=|<|>)(-?\d+(\.\d+)?)$/))) {
			const op = { '<': 'lt', '<=': 'lte', '>': 'gt', '>=': 'gte' }[m[2]] as string;
			must.push({ key: m[1], range: { [op]: Number(m[3]) } });
		} else if ((m = part.match(/^([^:\s]+)[:=](.+)$/))) {
			must.push({ key: m[1], match: value(m[2]) });
		} else {
			words.push(part.toLowerCase());
		}
	}
	if (ids.length) must.push({ has_id: ids });
	return { filter: must.length ? { must } : null, words };
}

/** Whether a payload holds every word, anywhere in its keys or values. */
export function matchesWords(payload: Payload | undefined, words: string[]): boolean {
	if (!words.length) return true;
	const text = JSON.stringify(payload ?? {}).toLowerCase();
	return words.every((w) => text.includes(w));
}
