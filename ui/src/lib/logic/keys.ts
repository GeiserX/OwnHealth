/** True when keys typed now go into a field (input, textarea, select, contenteditable), so a page shortcut must not take them. */
export function isTypingTarget(el: { tagName?: string; isContentEditable?: boolean } | null | undefined): boolean {
	if (!el) return false;
	const tag = (el.tagName ?? '').toUpperCase();
	return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || !!el.isContentEditable;
}
