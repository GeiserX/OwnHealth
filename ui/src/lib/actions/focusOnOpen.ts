// The one behaviour of every inline form or panel that opens on a click: it scrolls into view and its
// first field takes focus; Escape closes it through its own cancel handler, and focus goes back to the
// button that opened it. Usage: <form use:focusOnOpen={{ oncancel }}>.
import type { Action } from 'svelte/action';

const FIELD = 'input:not([type=hidden]):not([disabled]), select:not([disabled]), textarea:not([disabled])';

export const focusOnOpen: Action<HTMLElement, { oncancel?: () => void } | undefined> = (node, params) => {
	let opts = params;
	const opener = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
	// After the browser has laid the panel out, so the scroll lands on its final place.
	const raf = requestAnimationFrame(() => {
		node.scrollIntoView?.({ block: 'start', behavior: 'smooth' });
		(node.querySelector<HTMLElement>(FIELD) ?? node.querySelector<HTMLElement>('button:not([disabled])'))?.focus({ preventScroll: true });
	});
	function onKey(e: KeyboardEvent) {
		if (e.key !== 'Escape' || !opts?.oncancel) return;
		e.preventDefault();
		e.stopPropagation();
		opts.oncancel();
	}
	node.addEventListener('keydown', onKey);
	return {
		update(p) {
			opts = p;
		},
		destroy() {
			cancelAnimationFrame(raf);
			node.removeEventListener('keydown', onKey);
			// Closed with focus inside (or lost with it): give it back to the opener when it is still there.
			const a = document.activeElement;
			if (opener?.isConnected && (!a || a === document.body || node.contains(a))) opener.focus();
		}
	};
};
