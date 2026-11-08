/**
 * A polling loop that can be stopped. `start()` stops the previous loop first, so two loops never poll
 * at once; `stop()` (call it on unmount) ends the current one, including an answer still in flight.
 * `tick` returns true when the work is finished, false to poll again, and throws on a failed fetch;
 * after `maxFails` failures in a row the loop gives up and calls `onfail`.
 */
export function poller(opts: {
	tick: () => Promise<boolean>;
	ondone: () => void;
	onfail: (e: unknown) => void;
	every?: number;
	maxFails?: number;
	timers?: { set: (f: () => void, ms: number) => unknown; clear: (h: unknown) => void };
}): { start: () => void; stop: () => void } {
	const every = opts.every ?? 2000;
	const maxFails = opts.maxFails ?? 5;
	const timers = opts.timers ?? {
		set: (f: () => void, ms: number) => setTimeout(f, ms),
		clear: (h: unknown) => clearTimeout(h as ReturnType<typeof setTimeout>)
	};
	let run = 0;
	let timer: unknown;
	const stop = () => {
		run++;
		if (timer !== undefined) timers.clear(timer);
		timer = undefined;
	};
	const step = async (mine: number, fails: number) => {
		timer = undefined;
		let done: boolean;
		try {
			done = await opts.tick();
		} catch (e) {
			if (mine !== run) return;
			if (fails + 1 >= maxFails) return opts.onfail(e);
			timer = timers.set(() => step(mine, fails + 1), every);
			return;
		}
		if (mine !== run) return;
		if (done) opts.ondone();
		else timer = timers.set(() => step(mine, 0), every);
	};
	return {
		start() {
			stop();
			step(run, 0);
		},
		stop
	};
}
