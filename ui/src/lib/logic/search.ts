// Where the top-bar search sends a typed term. The term always travels on, so no page opens unfiltered.
export function searchTarget(term: string): string | null {
	const v = term.trim();
	if (!v) return null;
	const q = encodeURIComponent(v);
	const l = v.toLowerCase();
	if (/clopi|drug|warf|codein|fármaco|farmaco|statin|estatin|omepra|ibupro/.test(l)) return `/drug-check?drug=${q}`;
	// The genome page has no term filter yet, so the term stops there.
	if (/^(apoe|hfe|cyp|gene|gen$|genom|rs\d)/.test(l)) return '/genome';
	if (/ldl|hdl|chol|colest|hba1c|gluc|ferrit|tsh|vitamin|creat|triglic|trigly/.test(l)) return `/biomarkers?q=${q}`;
	return `/documents?q=${q}`;
}
