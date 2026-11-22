// The QR code of a share link: Nayuki's encoder (qrcodegen.ts), error correction M, drawn as one SVG
// path by logic.ts qrPath.
import { qrPath } from './logic';
import { qrcodegen } from './qrcodegen';

export const QUIET = 4;

export function qrOf(text: string): { size: number; path: string } {
	const qr = qrcodegen.QrCode.encodeText(text, qrcodegen.QrCode.Ecc.MEDIUM);
	return { size: qr.size + 2 * QUIET, path: qrPath(qr.size, (x, y) => qr.getModule(x, y), QUIET) };
}
