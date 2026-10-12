// Reads the Prometheus text the server's /metrics endpoint writes.

export type Sample = { name: string; labels: Record<string, string>; value: number };

export function parse(text: string): Sample[] {
	const out: Sample[] = [];
	for (const line of text.split('\n')) {
		if (!line || line.startsWith('#')) continue;
		const m = line.match(/^([a-zA-Z_:][\w:]*)(\{(.*)\})?\s+(\S+)/);
		if (!m) continue;
		const labels: Record<string, string> = {};
		if (m[3]) {
			for (const l of m[3].matchAll(/(\w+)="((?:[^"\\]|\\.)*)"/g)) {
				labels[l[1]] = l[2].replace(/\\(.)/g, (_, c) => (c === 'n' ? '\n' : c));
			}
		}
		out.push({ name: m[1], labels, value: Number(m[4]) });
	}
	return out;
}

export type RouteStats = {
	route: string;
	requests: number;
	clientErrors: number;
	serverErrors: number;
	meanMs: number;
	p95Ms: number | null;
};

/** Requests and latency per route, busiest first. */
export function routes(samples: Sample[]): RouteStats[] {
	const by = new Map<string, RouteStats & { sum: number; count: number; buckets: [number, number][] }>();
	const get = (route: string) => {
		let r = by.get(route);
		if (!r) {
			r = { route, requests: 0, clientErrors: 0, serverErrors: 0, meanMs: 0, p95Ms: null, sum: 0, count: 0, buckets: [] };
			by.set(route, r);
		}
		return r;
	};
	for (const s of samples) {
		const route = s.labels.route;
		if (!route) continue;
		if (s.name === 'nocturnum_http_requests_total') {
			const r = get(route);
			r.requests += s.value;
			const status = Number(s.labels.status);
			if (status >= 500) r.serverErrors += s.value;
			else if (status >= 400) r.clientErrors += s.value;
		} else if (s.name === 'nocturnum_http_request_duration_seconds_sum') {
			get(route).sum = s.value;
		} else if (s.name === 'nocturnum_http_request_duration_seconds_count') {
			get(route).count = s.value;
		} else if (s.name === 'nocturnum_http_request_duration_seconds_bucket') {
			const le = s.labels.le === '+Inf' ? Infinity : Number(s.labels.le);
			get(route).buckets.push([le, s.value]);
		}
	}
	return [...by.values()]
		.map((r) => {
			r.meanMs = r.count ? (r.sum / r.count) * 1000 : 0;
			const target = r.count * 0.95;
			const hit = r.buckets.sort((a, b) => a[0] - b[0]).find(([, n]) => n >= target);
			r.p95Ms = hit && Number.isFinite(hit[0]) && r.count ? hit[0] * 1000 : null;
			const { route, requests, clientErrors, serverErrors, meanMs, p95Ms } = r;
			return { route, requests, clientErrors, serverErrors, meanMs, p95Ms };
		})
		.filter((r) => r.requests > 0)
		.sort((a, b) => b.requests - a.requests);
}

/** One value per collection for a metric labelled by collection. */
export function byCollection(samples: Sample[], name: string): Map<string, number> {
	const out = new Map<string, number>();
	for (const s of samples) if (s.name === name && s.labels.collection) out.set(s.labels.collection, s.value);
	return out;
}
