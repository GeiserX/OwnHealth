// The timeline's bars lane (slice 6). Its axis ends today, so a course that starts later has no bar:
// drawn, it would sit at today's edge as if it ran now. The phone list still shows it with its date.
import { buildBars, type Annotation, type Bar } from './axis.ts';

export function laneBars<G extends Annotation>(segments: G[], x: (d: string) => number, today: string): { b: Bar; g: G }[] {
	const started = segments.filter((g) => g.date <= today);
	return buildBars(started, x, today).map((b, i) => ({ b, g: started[i] }));
}
