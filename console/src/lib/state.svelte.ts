// What every page shares: the server, the collections, the key and the top search.

import { api, ApiError, storeKey, storedKey, type CollectionSummary, type Node } from './api';

class Console {
	node = $state<Node | null>(null);
	collections = $state<CollectionSummary[]>([]);
	/** Set once the first refresh has answered, either way. */
	loaded = $state(false);
	/** Why the server cannot be reached or refuses this browser, `null` while all is well. */
	problem = $state<ApiError | null>(null);
	/** Why `/readyz` says the server is not ready, such as a collection a failed delete left out
	 * of service, which the collection list cannot show, `null` while it is ready. */
	notReady = $state<string | null>(null);
	/** What is typed in the top search. */
	search = $state('');
	hasKey = $state(storedKey() !== null);
	/** Bumped after a change, so pages reload what they show. */
	changes = $state(0);

	get role() {
		return this.node?.role ?? null;
	}

	/** Whether this browser may change things. */
	get canWrite() {
		return this.role === 'admin' || this.role === 'anonymous';
	}

	/** Bumped when the key changes, so answers to requests sent with the old key are dropped. */
	#generation = 0;

	/** Numbers each refresh, so an older one that answers late cannot undo a newer one. */
	#latest = 0;

	#pending = 0;

	/** A routine refresh, skipped while one is still out. */
	poll() {
		if (this.#pending === 0) this.refresh();
	}

	async refresh() {
		const mine = this.#generation;
		const seq = ++this.#latest;
		this.#pending++;
		const stale = () => mine !== this.#generation || seq !== this.#latest;
		const ready = api.ready().then(
			() => null,
			(e) => (e instanceof ApiError && e.status !== 0 ? e.message : null)
		);
		try {
			const [node, list, notReady] = await Promise.all([api.node(), api.collections(), ready]);
			if (stale()) return;
			this.notReady = notReady;
			this.node = node;
			this.collections = list.collections;
			this.problem = null;
		} catch (e) {
			if (stale()) return;
			this.problem = e instanceof ApiError ? e : new ApiError(0, 'unknown', String(e));
		} finally {
			this.#pending--;
			if (!stale()) this.loaded = true;
		}
	}

	setKey(key: string | null) {
		this.#generation++;
		storeKey(key);
		this.hasKey = key !== null;
		this.node = null;
		this.collections = [];
		this.problem = null;
		this.changes++;
		return this.refresh();
	}

	changed() {
		this.changes++;
		return this.refresh();
	}
}

export const app = new Console();

type Toast = { id: number; text: string; tone: 'ok' | 'bad' };

class Toasts {
	list = $state<Toast[]>([]);
	#next = 0;

	show(text: string, tone: Toast['tone'] = 'ok') {
		const id = this.#next++;
		this.list.push({ id, text, tone });
		setTimeout(() => this.dismiss(id), tone === 'bad' ? 8000 : 3500);
	}

	error(e: unknown) {
		this.show(e instanceof Error ? e.message : String(e), 'bad');
	}

	dismiss(id: number) {
		this.list = this.list.filter((t) => t.id !== id);
	}
}

export const toasts = new Toasts();
