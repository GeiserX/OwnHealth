// Opening one study in the viewer, from the Imaging page and from the timeline (slice 42).
import { openStudy } from '#lib/api';

/**
 * Asks for a one-hour viewer link and opens it in a new tab. The tab opens on the click itself, so a
 * popup blocker lets it through, and goes to the viewer once the server answers. On a refusal the tab
 * closes again and the error is thrown to the caller, which shows it. The viewer runs sandboxed in an
 * opaque origin; the tab also drops its opener before it goes there.
 */
export async function openViewer(profile: string, uid: string): Promise<void> {
	const tab = window.open('', '_blank');
	try {
		const { viewer_url } = await openStudy(profile, uid);
		const url = new URL(viewer_url, window.location.origin).href;
		if (tab) {
			tab.opener = null;
			tab.location.href = url;
		} else window.location.assign(url);
	} catch (e) {
		tab?.close();
		throw e;
	}
}
