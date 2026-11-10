// Where the top-bar search sends a typed term. The term always travels on, so no page opens unfiltered.
export function searchTarget(term: string): string | null {
	const v = term.trim();
	if (!v) return null;
	const q = encodeURIComponent(v);
	const l = v.toLowerCase();
	if (/clopi|drug|warf|codein|fármaco|farmaco|statin|estatin|omepra|ibupro/.test(l)) return `/drug-check?drug=${q}`;
	if (/^(apoe|hfe|cyp|gene|gen$|genom|rs\d)/.test(l)) return `/genome?q=${q}`;
	// Long names match anywhere; short ones (ALT, T4, PCR…) only as a whole word, so 'salt' stays a search.
	if (/ldl|hdl|chol|colest|hba1c|gluc|ferrit|tsh|vitamin|creat|triglic|trigly|lp\(a\)|lipoprot|hemoglob|plaquet|platelet|leuco|egfr|filtrado|albúm|albumin/.test(l)) return `/biomarkers?q=${q}`;
	if (/(^|[^\p{L}\d])(lpa|pcr|crp|alt|ast|ggt|vcm|mcv|b12|t4)($|[^\p{L}\d])/u.test(l)) return `/biomarkers?q=${q}`;
	return `/documents?q=${q}`;
}
