// GEDCOM import (slice 24): what the browser does with the person's own tree file. The server reads
// only structure and never a name (ownhealth-core/src/gedcom.rs). The picker still shows names: this
// file reads each INDI record's first NAME here, in the browser, numbered the way the server numbers
// people (the n-th INDI record of the file), and never sends them anywhere.

/** The largest tree file the server reads (gedcom::MAX_BYTES): a larger one is refused before it is read. */
export const MAX_TREE_BYTES = 10 * 1024 * 1024;

// ANSEL (ANSI/NISO Z39.47), the character set of many GEDCOM 5.5.1 exports: its accents come before
// their letter, and its letters past ASCII sit at 0xA1 to 0xC8.
const ANSEL_MARKS: Record<number, number> = {
	0xe0: 0x309, 0xe1: 0x300, 0xe2: 0x301, 0xe3: 0x302, 0xe4: 0x303, 0xe5: 0x304, 0xe6: 0x306, 0xe7: 0x307,
	0xe8: 0x308, 0xe9: 0x30c, 0xea: 0x30a, 0xeb: 0xfe20, 0xec: 0xfe21, 0xed: 0x315, 0xee: 0x30b, 0xef: 0x310,
	0xf0: 0x327, 0xf1: 0x328, 0xf2: 0x323, 0xf3: 0x324, 0xf4: 0x325, 0xf5: 0x333, 0xf6: 0x332, 0xf7: 0x326,
	0xf8: 0x31c, 0xf9: 0x32e, 0xfa: 0xfe22, 0xfb: 0xfe23, 0xfe: 0x313
};
const ANSEL_LETTERS = 'ŁØĐÞÆŒʹ·♭®±ƠƯʼ\uFFFDʻłøđþæœʺı£ð\uFFFDơư\uFFFD\uFFFD°ℓ℗©♯¿¡ß€';

function decodeAnsel(bytes: Uint8Array): string {
	let out = '';
	let marks = '';
	for (const b of bytes) {
		const mark = ANSEL_MARKS[b];
		if (mark) {
			marks += String.fromCharCode(mark);
			continue;
		}
		out += (b < 0x80 ? String.fromCharCode(b) : b >= 0xa1 && b <= 0xc8 ? ANSEL_LETTERS[b - 0xa1] : '\uFFFD') + marks;
		marks = '';
	}
	return out.normalize('NFC');
}

/**
 * The text of a tree file: UTF-16 when it starts with a byte order mark, UTF-8 when it reads as UTF-8.
 * Anything else is a one-byte set: ANSEL when the header's `1 CHAR` says so, Windows-1252 (`ANSI`)
 * otherwise.
 */
export function decodeTree(bytes: Uint8Array): string {
	const b0 = bytes[0];
	const b1 = bytes[1];
	if (b0 === 0xff && b1 === 0xfe) return new TextDecoder('utf-16le').decode(bytes).replace(/^\uFEFF/, '');
	if (b0 === 0xfe && b1 === 0xff) return new TextDecoder('utf-16be').decode(bytes).replace(/^\uFEFF/, '');
	try {
		return new TextDecoder('utf-8', { fatal: true }).decode(bytes).replace(/^\uFEFF/, '');
	} catch {
		const ansi = new TextDecoder('windows-1252').decode(bytes);
		// CHAR sits in the header, at the top of the file
		return /^[ \t]*1 CHAR[ \t]+ANSEL\b/im.test(ansi.slice(0, 4096)) ? decodeAnsel(bytes) : ansi;
	}
}

/** `Alex /Rivera/` as a person reads it: `Alex Rivera`. */
export function cleanName(value: string): string | null {
	const name = value.replace(/\//g, ' ').replace(/\s+/g, ' ').trim();
	return name || null;
}

const LINE = /^(\d{1,2}) (?:@[^ ]+@ )?([A-Za-z0-9_]+)(?: (.*))?$/;

/** The first NAME of each INDI record, by position; null when a record has none. */
export function namesByPosition(text: string): (string | null)[] {
	const out: (string | null)[] = [];
	let inPerson = false;
	let named = false;
	for (const raw of text.split(/\r\n|\r|\n/)) {
		const m = LINE.exec(raw.replace(/^[ \t]+/, ''));
		if (!m) continue;
		const level = Number(m[1]);
		const tag = m[2];
		if (level === 0) {
			if (tag === 'TRLR') break;
			inPerson = tag === 'INDI';
			named = false;
			if (inPerson) out.push(null);
		} else if (inPerson && !named && level === 1 && tag === 'NAME' && m[3]) {
			out[out.length - 1] = cleanName(m[3]);
			named = true;
		}
	}
	return out;
}

export type PickPerson = { i: number; sex: string; born_year: number | null; deceased: boolean; death_age: number | null };

/**
 * The people a search shows in the picker: by a name the browser read, or by a birth year. Names are
 * used only when the browser counted the same people as the server (a file it decodes differently
 * shows no names rather than wrong ones).
 */
export function pickable<P extends PickPerson>(people: P[], names: (string | null)[], query: string, limit = 30): P[] {
	const useNames = names.length === people.length;
	const q = query.trim().toLowerCase();
	const hit = (p: P) => {
		if (!q) return true;
		if (p.born_year != null && String(p.born_year).startsWith(q)) return true;
		const n = useNames ? names[p.i] : null;
		return !!n && n.toLowerCase().includes(q);
	};
	return people.filter(hit).slice(0, limit);
}

/** The payload of a `family_proposal` review item. */
export type FamilyProposal = {
	relationship?: string;
	label?: Record<string, string>;
	sex_at_birth?: string | null;
	born_year?: number | null;
	deceased?: boolean;
	death_age?: number | null;
};

/** The relationships whose name says no sex (relationships.yaml `sex: null`) that a tree can propose. */
const NO_SEX = new Set(['PRN', 'SPS', 'CHILD', 'SIB', 'HSIB', 'PCOUSN', 'MCOUSN']);

type Word = (key: string, vars?: Record<string, string | number>) => string;

/**
 * The line a family proposal shows in the review queue: the relationship in the reader's language, the
 * sex when the relationship does not say it, the birth year and the death. `word(key, vars)` is the
 * page's tf().
 */
export function proposalTitle(p: FamilyProposal, lang: string, word: Word): string {
	const rel = p.label?.[lang] ?? p.label?.en ?? p.relationship ?? '';
	const parts = [rel ? rel.charAt(0).toUpperCase() + rel.slice(1) : word('i2_kind_family_proposal')];
	if (p.relationship && NO_SEX.has(p.relationship) && p.sex_at_birth)
		parts.push(word(p.sex_at_birth === 'female' ? 'gd_sex_f' : p.sex_at_birth === 'male' ? 'gd_sex_m' : 'gd_sex_u'));
	if (p.born_year != null) parts.push(word('gd_born', { y: p.born_year }));
	if (p.death_age != null) parts.push(word('gd_died_at', { n: p.death_age }));
	else if (p.deceased) parts.push(word('gd_died'));
	return parts.join(' · ');
}

/** The line at the end of an import: how many relatives now wait in Import & review. */
export function doneLine(n: number, word: Word): string {
	return n === 0 ? word('gd_done_none') : n === 1 ? word('gd_done_one') : word('gd_done', { n });
}
