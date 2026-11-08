// Which source the source panel shows. SourceChip's `onopen` calls openSource; SourcePanel reads it.
export const sourcePanel = $state<{ src: string | null }>({ src: null });

export function openSource(src: string): void {
	sourcePanel.src = src;
}
