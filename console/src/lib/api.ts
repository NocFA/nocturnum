// The nocturnum REST API, as docs/api/v1.md describes it, with the key kept in this browser.

export type Distance = 'cosine' | 'dot' | 'euclid' | 'manhattan';
/** Ids above 2^53 − 1 arrive as strings of digits (docs/api/v1.md, decision 7), so they never
 * pass through a float and are sent back exactly as they came. */
export type PointId = number | string;
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Payload = { [key: string]: Json };
export type Role = 'admin' | 'read-only' | 'anonymous';

export interface VectorConfig {
	dim: number;
	distance: Distance;
}

export interface Disk {
	total_bytes: number;
	available_bytes: number;
}

export interface Node {
	name: string;
	version: string;
	started_at: string;
	uptime_seconds: number;
	role: Role;
	keys: { admin: boolean; read_only: boolean };
	disk: { collections: Disk | null; snapshots: Disk | null };
}

export interface CollectionSummary {
	name: string;
	points: number;
	vectors: VectorConfig;
	segments: number;
	failure: string | null;
}

export interface IndexSpec {
	path: string;
	kind: 'keyword' | 'integer' | 'float' | 'bool';
}

export interface CollectionInfo {
	name: string;
	vectors: VectorConfig;
	points: number;
	segments: number;
	indexes: IndexSpec[];
}

export interface SegmentStatus {
	id: number;
	rows: number;
	live: number;
	index: 'hnsw' | 'plain';
	graph_bytes: number;
	vector_bytes: number;
	disk_bytes: number;
}

export interface Job {
	kind: 'flush' | 'merge' | 'vacuum';
	inputs?: number[];
	output?: number;
}

export interface CollectionStatus {
	points: number;
	failure: string | null;
	segments: SegmentStatus[];
	memtable: { entries: number; bytes: number; flush_bytes: number; frozen_entries: number };
	wal: { bytes: number; last_seq: number; fsyncs: number };
	index: { m: number; ef_construct: number; min_bytes: number; default_ef: number };
	optimizer: {
		enabled: boolean;
		max_segments: number | null;
		vacuum_ratio: number | null;
		running: number[] | null;
		last: Job | null;
		last_error: string | null;
	};
	disk_bytes: number;
}

export interface Point {
	id: PointId;
	payload?: Payload;
	vector?: number[];
}

export interface Scored extends Point {
	score: number;
}

export interface Page {
	points: Point[];
	next: PointId | null;
}

export interface RecallRun {
	hnsw_ef: number;
	recall: number | null;
	p50_ms: number;
	p99_ms: number;
}

export interface RecallReport {
	samples: number;
	limit: number;
	points: number;
	indexed_points: number;
	exact: { p50_ms: number; p99_ms: number };
	runs: RecallRun[];
}

export interface Snapshot {
	name: string;
	size: number;
	sha256: string;
	created_at: string;
}

export interface ScheduleState {
	cron: string;
	keep: number;
	next_run: string | null;
	running: boolean;
	last_run: string | null;
	last_success: string | null;
	last_snapshot: string | null;
	last_error: string | null;
	damaged: string[];
}

export interface WriteResult {
	seq: number;
	ack: string;
}

/** An error the server answered with, or a network failure with status 0. */
export class ApiError extends Error {
	constructor(
		public status: number,
		public code: string,
		message: string,
		public at?: string
	) {
		super(message);
	}
}

const KEY = 'nocturnum.key';

export function storedKey(): string | null {
	try {
		return localStorage.getItem(KEY);
	} catch {
		return null;
	}
}

export function storeKey(key: string | null) {
	try {
		if (key) localStorage.setItem(KEY, key);
		else localStorage.removeItem(KEY);
	} catch {
		// Private windows may refuse storage, and the key then lasts until the tab closes.
	}
	memoryKey = key;
}

let memoryKey: string | null = null;

function key(): string | null {
	return memoryKey ?? storedKey();
}

async function request<T>(method: string, path: string, body?: unknown, raw = false): Promise<T> {
	const headers: Record<string, string> = {};
	const k = key();
	if (k) headers['Authorization'] = `Bearer ${k}`;
	if (body !== undefined) headers['Content-Type'] = 'application/json';
	let resp: Response;
	try {
		resp = await fetch(path, {
			method,
			headers,
			body: body === undefined ? undefined : JSON.stringify(body)
		});
	} catch {
		throw new ApiError(0, 'offline', 'the server did not answer, it may be down or restarting');
	}
	if (raw && resp.ok) return (await resp.text()) as T;
	const text = await resp.text();
	let value: unknown = null;
	try {
		value = text ? JSON.parse(text) : null;
	} catch {
		value = null;
	}
	if (!resp.ok) {
		const e = (value as { error?: { code: string; message: string; at?: string } })?.error;
		throw new ApiError(
			resp.status,
			e?.code ?? 'http_' + resp.status,
			e?.message ?? (text || resp.statusText),
			e?.at
		);
	}
	return value as T;
}

const c = (name: string) => `/v1/collections/${encodeURIComponent(name)}`;

export const api = {
	node: () => request<Node>('GET', '/v1'),
	/** `/readyz`, which needs no key and fails with the reason while a collection is out of service. */
	ready: () => request<{ status: string }>('GET', '/readyz'),
	metrics: () => request<string>('GET', '/metrics', undefined, true),
	collections: () => request<{ collections: CollectionSummary[] }>('GET', '/v1/collections'),
	collection: (name: string) => request<CollectionInfo>('GET', c(name)),
	status: (name: string) => request<CollectionStatus>('GET', `${c(name)}/status`),
	create: (name: string, vectors: VectorConfig) =>
		request<{ created: boolean }>('PUT', c(name), { vectors }),
	drop: (name: string) => request<{ deleted: boolean }>('DELETE', c(name)),

	scroll: (name: string, body: { filter?: Json; limit?: number; after?: PointId | null; with_vector?: boolean }) =>
		request<Page>('POST', `${c(name)}/points/scroll`, body),
	count: (name: string, filter?: Json) =>
		request<{ count: number }>('POST', `${c(name)}/points/count`, filter ? { filter } : {}),
	point: (name: string, id: PointId, withVector = false) =>
		request<Point>(
			'GET',
			`${c(name)}/points/${encodeURIComponent(String(id))}?with_vector=${withVector}`
		),
	search: (name: string, body: Json) =>
		request<{ points: Scored[] }>('POST', `${c(name)}/points/search`, body),
	recall: (name: string, body: Json) =>
		request<RecallReport>('POST', `${c(name)}/points/recall`, body),
	deletePoints: (name: string, ids: PointId[]) =>
		request<WriteResult>('POST', `${c(name)}/points/delete`, { ids }),
	overwritePayload: (name: string, id: PointId, payload: Payload) =>
		request<WriteResult>('POST', `${c(name)}/points/payload/overwrite`, { ids: [id], payload }),

	createIndex: (name: string, spec: IndexSpec) =>
		request<{ created: boolean }>('PUT', `${c(name)}/indexes`, spec),
	dropIndex: (name: string, spec: IndexSpec) =>
		request<{ deleted: boolean }>('POST', `${c(name)}/indexes/delete`, spec),

	snapshots: (name: string) => request<{ snapshots: Snapshot[] }>('GET', `${c(name)}/snapshots`),
	takeSnapshot: (name: string) => request<Snapshot>('POST', `${c(name)}/snapshots`),
	deleteSnapshot: (name: string, snap: string) =>
		request<{ deleted: boolean }>('DELETE', `${c(name)}/snapshots/${encodeURIComponent(snap)}`),
	restore: (name: string, snapshot: string) =>
		request<{ restored: boolean }>('POST', `${c(name)}/restore`, { snapshot }),
	schedule: (name: string) => request<ScheduleState>('GET', `${c(name)}/snapshots/schedule`),
	setSchedule: (name: string, cron: string, keep: number) =>
		request<ScheduleState>('PUT', `${c(name)}/snapshots/schedule`, { cron, keep }),
	deleteSchedule: (name: string) =>
		request<{ deleted: boolean }>('DELETE', `${c(name)}/snapshots/schedule`)
};

/** Downloads a snapshot through fetch, so the key goes in a header rather than the URL. */
export async function downloadSnapshot(name: string, snap: string) {
	const headers: Record<string, string> = {};
	const k = key();
	if (k) headers['Authorization'] = `Bearer ${k}`;
	const resp = await fetch(`${c(name)}/snapshots/${encodeURIComponent(snap)}`, { headers });
	if (!resp.ok) throw new ApiError(resp.status, 'download_failed', `download failed with ${resp.status}`);
	const blob = await resp.blob();
	const url = URL.createObjectURL(blob);
	const a = document.createElement('a');
	a.href = url;
	a.download = snap;
	a.click();
	setTimeout(() => URL.revokeObjectURL(url), 10_000);
}
