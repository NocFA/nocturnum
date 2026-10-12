// Numbered reads per key, such as one collection's schedule, so an answer overtaken by a newer
// read of the same thing is dropped, and a routine poll waits for the last read to answer.

export class Streams {
	#latest = new Map<string, number>();
	#pending = new Map<string, number>();

	/** A ticket for a new read of `key`, or `null` for a routine read while one is still out. */
	start(key: string, routine: boolean): number | null {
		if (routine && (this.#pending.get(key) ?? 0) > 0) return null;
		const n = (this.#latest.get(key) ?? 0) + 1;
		this.#latest.set(key, n);
		this.#pending.set(key, (this.#pending.get(key) ?? 0) + 1);
		return n;
	}

	/** Ends a read, returning whether its answer is still the newest and may be used. */
	finish(key: string, n: number): boolean {
		this.#pending.set(key, Math.max(0, (this.#pending.get(key) ?? 1) - 1));
		return this.#latest.get(key) === n;
	}
}
