// WebAuthn in the browser with the JSON the backend sends (py_webauthn options_to_json, base64url) and
// wants back (PublicKeyCredential.toJSON()). Uses the native JSON helpers where the browser has them.
type Json = Record<string, unknown>;

const b64uToBuf = (s: string): ArrayBuffer => {
	const b = atob(s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4));
	const out = new Uint8Array(b.length);
	for (let i = 0; i < b.length; i++) out[i] = b.charCodeAt(i);
	return out.buffer;
};
const bufToB64u = (buf: ArrayBuffer): string => {
	let s = '';
	for (const c of new Uint8Array(buf)) s += String.fromCharCode(c);
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
};
const creds = (list: unknown) => ((list as Json[] | undefined) ?? []).map((c) => ({ ...c, id: b64uToBuf(c.id as string) }));

export function creationOptions(o: Json): PublicKeyCredentialCreationOptions {
	const user = o.user as Json;
	return {
		...(o as object),
		challenge: b64uToBuf(o.challenge as string),
		user: { ...user, id: b64uToBuf(user.id as string) },
		excludeCredentials: creds(o.excludeCredentials)
	} as unknown as PublicKeyCredentialCreationOptions;
}

export function requestOptions(o: Json): PublicKeyCredentialRequestOptions {
	return { ...(o as object), challenge: b64uToBuf(o.challenge as string), allowCredentials: creds(o.allowCredentials) } as unknown as PublicKeyCredentialRequestOptions;
}

export function credentialJson(c: PublicKeyCredential): Json {
	const native = (c as unknown as { toJSON?: () => Json }).toJSON;
	if (typeof native === 'function') return native.call(c);
	const r = c.response as AuthenticatorAttestationResponse & AuthenticatorAssertionResponse;
	const response: Json = { clientDataJSON: bufToB64u(r.clientDataJSON) };
	if ('attestationObject' in r && r.attestationObject) {
		response.attestationObject = bufToB64u(r.attestationObject);
		response.transports = r.getTransports?.() ?? [];
	} else {
		response.authenticatorData = bufToB64u(r.authenticatorData);
		response.signature = bufToB64u(r.signature);
		if (r.userHandle) response.userHandle = bufToB64u(r.userHandle);
	}
	return { id: c.id, rawId: bufToB64u(c.rawId), type: c.type, response, clientExtensionResults: c.getClientExtensionResults(), authenticatorAttachment: c.authenticatorAttachment };
}

/** Create a passkey from the backend's registration options. */
export async function createPasskey(options: Json): Promise<Json> {
	const c = (await navigator.credentials.create({ publicKey: creationOptions(options) })) as PublicKeyCredential | null;
	if (!c) throw new Error('cancelled');
	return credentialJson(c);
}

/** Sign in with a passkey from the backend's login options. */
export async function usePasskey(options: Json): Promise<Json> {
	const c = (await navigator.credentials.get({ publicKey: requestOptions(options) })) as PublicKeyCredential | null;
	if (!c) throw new Error('cancelled');
	return credentialJson(c);
}

export const _test = { b64uToBuf, bufToB64u };
