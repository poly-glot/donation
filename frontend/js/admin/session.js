import { config, form } from "../api.js";

const PENDING = "admin-pending";
const SESSION = "admin-session";

const LOST_PENDING = "The sign-in did not start from this tab. Sign in again.";
const WRONG_STATE = "The sign-in reply did not match the request it answers. Sign in again.";

const urlBase64 = (bytes) => btoa(String.fromCharCode(...new Uint8Array(bytes))).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
const held = (key) => JSON.parse(sessionStorage.getItem(key) ?? "null");
const hold = (key, value) => sessionStorage.setItem(key, JSON.stringify(value));

const digest = async (text) => urlBase64(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
const here = () => `${location.origin}${location.pathname}`;

async function exchange(code, verifier) {
    const granted = await form(`${config.cognito.domain}/oauth2/token`, {
        client_id: config.cognito.clientId,
        code,
        code_verifier: verifier,
        grant_type: "authorization_code",
        redirect_uri: here(),
    });

    return { expiresAt: Date.now() + granted.expires_in * 1000, token: granted.access_token };
}

export const token = () => held(SESSION)?.token;

export async function signIn() {
    const verifier = urlBase64(crypto.getRandomValues(new Uint8Array(32)));
    const state = urlBase64(crypto.getRandomValues(new Uint8Array(16)));

    hold(PENDING, { state, verifier });

    const query = new URLSearchParams({
        client_id: config.cognito.clientId,
        code_challenge: await digest(verifier),
        code_challenge_method: "S256",
        redirect_uri: here(),
        response_type: "code",
        scope: "openid",
        state,
    });

    location.assign(`${config.cognito.domain}/oauth2/authorize?${query}`);
}

export function signOut() {
    sessionStorage.removeItem(SESSION);
    sessionStorage.removeItem(PENDING);

    location.assign(here());
}

export async function restore() {
    const reply = new URLSearchParams(location.search);
    const code = reply.get("code");

    if (!code) {
        return held(SESSION)?.expiresAt > Date.now();
    }

    const pending = held(PENDING);
    sessionStorage.removeItem(PENDING);
    history.replaceState(null, "", here() + location.hash);

    if (!pending) {
        throw new Error(LOST_PENDING);
    }
    if (pending.state !== reply.get("state")) {
        throw new Error(WRONG_STATE);
    }

    hold(SESSION, await exchange(code, pending.verifier));
    return true;
}
