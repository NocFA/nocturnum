// Numbers, sizes and times the way the console shows them, with tabular numerals in CSS.

const whole = new Intl.NumberFormat('en-GB');

/** 48210 as "48,210". */
export function int(n: number): string {
	return whole.format(n);
}

/** 48210 as "48.2k", for tight places such as the sidebar. */
export function compact(n: number): string {
	if (n < 1000) return String(n);
	if (n < 1_000_000) return trim(n / 1000) + 'k';
	if (n < 1_000_000_000) return trim(n / 1_000_000) + 'M';
	return trim(n / 1_000_000_000) + 'B';
}

function trim(x: number): string {
	return x >= 100 ? x.toFixed(0) : x.toFixed(1).replace(/\.0$/, '');
}

const units = ['B', 'KB', 'MB', 'GB', 'TB'];

/** Bytes in decimal units, "1.48 GB", the way disks are sold. */
export function bytes(n: number): string {
	let i = 0;
	let x = n;
	while (x >= 1000 && i < units.length - 1) {
		x /= 1000;
		i++;
	}
	if (i === 0) return `${n} B`;
	return `${x >= 100 ? x.toFixed(0) : x >= 10 ? x.toFixed(1) : x.toFixed(2)} ${units[i]}`;
}

/** Splits bytes into a number and a unit, for a figure set large beside a small unit. */
export function bytesParts(n: number): [string, string] {
	const s = bytes(n);
	const at = s.lastIndexOf(' ');
	return [s.slice(0, at), s.slice(at + 1)];
}

export function percent(x: number, digits = 1): string {
	return (x * 100).toFixed(digits) + '%';
}

export function ms(x: number): string {
	if (x < 1) return x.toFixed(2);
	if (x < 10) return x.toFixed(1);
	return x.toFixed(0);
}

/** "18 d 4 h", "3 h 12 min", "42 s". */
export function duration(seconds: number): string {
	const d = Math.floor(seconds / 86400);
	const h = Math.floor((seconds % 86400) / 3600);
	const m = Math.floor((seconds % 3600) / 60);
	if (d > 0) return `${d} d ${h} h`;
	if (h > 0) return `${h} h ${m} min`;
	if (m > 0) return `${m} min`;
	return `${Math.floor(seconds)} s`;
}

/** "14 min ago", "in 3 h", against `now`. */
export function relative(iso: string | null | undefined, now = Date.now()): string {
	if (!iso) return 'never';
	const t = Date.parse(iso);
	if (Number.isNaN(t)) return iso;
	const s = Math.round((t - now) / 1000);
	const a = Math.abs(s);
	const text = a < 5 ? 'now' : duration(a);
	if (text === 'now') return 'just now';
	return s < 0 ? `${text} ago` : `in ${text}`;
}

/** "2026-10-10 03:00" in UTC, which is what cron schedules and snapshot names use. */
export function utc(iso: string | null | undefined): string {
	if (!iso) return '';
	return iso.replace('T', ' ').replace(/:\d\d(\.\d+)?Z$/, '');
}

export function plural(n: number, one: string, many = one + 's'): string {
	return `${int(n)} ${n === 1 ? one : many}`;
}

/** An object's own entry for a key, so names like "constructor" never find an inherited value. */
export function own<T>(map: Record<string, T>, key: string): T | undefined {
	return Object.hasOwn(map, key) ? map[key] : undefined;
}
