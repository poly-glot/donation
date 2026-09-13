const BASE = "/api";

export const config = await import("../config.js").then((module) => module.default).catch(() => null);

async function send(url, init) {
    const response = await fetch(url, init);
    const payload = await response.json().catch(() => ({}));

    if (!response.ok) {
        throw new Error(payload.error ?? `${response.status} from ${url}`);
    }

    return payload;
}

const posted = (body) => ({
    body: JSON.stringify(body),
    headers: { "content-type": "application/json" },
    method: "POST",
});

export const call = (path, init) => send(BASE + path, init);
export const form = (url, fields) =>
    send(url, {
        body: new URLSearchParams(fields),
        headers: { "content-type": "application/x-www-form-urlencoded" },
        method: "POST",
    });
export const invoke = (path, request, token) =>
    send(BASE + path, { ...posted(request), headers: { authorization: `Bearer ${token}`, "content-type": "application/json" } });
export const post = (path, body) => call(path, posted(body));
