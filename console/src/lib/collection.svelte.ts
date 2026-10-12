// One collection's description, engine status and snapshots, shared by its tabs.

import { getContext, setContext } from 'svelte';
import { api, ApiError, type CollectionInfo, type CollectionStatus, type Snapshot } from './api';

export class CollectionView {
	info = $state<CollectionInfo | null>(null);
	status = $state<CollectionStatus | null>(null);
	snapshots = $state<Snapshot[] | null>(null);
	error = $state<ApiError | null>(null);

	constructor(public name: string) {}

	// Each read stream numbers its requests, so an answer overtaken by a newer read, such as the
	// one a change makes after it lands, is dropped.
	#loads = 0;
	#snapshotLoads = 0;
	#pending = 0;

	/** A routine refresh, skipped while a read is still out, so a slow server is not asked
	 * again before it answers and each answer gets to land. */
	poll() {
		if (this.#pending === 0) this.load();
	}

	async load() {
		const mine = ++this.#loads;
		this.#pending++;
		try {
			const [info, status] = await Promise.all([api.collection(this.name), api.status(this.name)]);
			if (mine !== this.#loads) return;
			this.info = info;
			this.status = status;
			this.error = null;
		} catch (e) {
			if (mine !== this.#loads) return;
			this.error = e instanceof ApiError ? e : new ApiError(0, 'unknown', String(e));
		} finally {
			this.#pending--;
		}
	}

	#snapshotsPending = 0;

	/** A routine refresh of the snapshot list, skipped while one is still out. */
	pollSnapshots() {
		if (this.#snapshotsPending === 0) this.loadSnapshots();
	}

	async loadSnapshots() {
		const mine = ++this.#snapshotLoads;
		this.#snapshotsPending++;
		try {
			const list = (await api.snapshots(this.name)).snapshots;
			if (mine === this.#snapshotLoads) this.snapshots = list;
		} catch {
			if (mine === this.#snapshotLoads) this.snapshots = null;
		} finally {
			this.#snapshotsPending--;
		}
	}
}

const KEY = Symbol('collection');

export function provide(view: CollectionView) {
	setContext(KEY, view);
}

export function current(): CollectionView {
	return getContext<CollectionView>(KEY);
}
