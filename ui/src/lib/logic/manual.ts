// What a field of the hand-entry form (ManualEntry.svelte) sends. Svelte's bind:value gives a string
// for a text or date input, but a number, or null when it is empty, for a number input (a vaccine's
// dose number). A correction pre-fills every field as text, and a field never touched is undefined.

export type FieldValue = string | number | null | undefined;

/** The field as trimmed text, or null when it is empty. 0 is text "0", not empty. */
export function fieldText(v: FieldValue): string | null {
	return v == null ? null : String(v).trim() || null;
}

/** The field as a number, or null when it is empty or not a number. 0 is a number. */
export function fieldNumber(v: FieldValue): number | null {
	const s = fieldText(v);
	const n = s == null ? Number.NaN : Number(s);
	return Number.isFinite(n) ? n : null;
}
