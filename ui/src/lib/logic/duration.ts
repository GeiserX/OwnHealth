/** '7 h 59 min' from decimal hours, rounded to the whole minute first so it never reads '7 h 60 min'. */
export function hoursMinutes(hours: number): string {
	const total = Math.round(hours * 60);
	return `${Math.floor(total / 60)} h ${total % 60} min`;
}
