const HTML_ESCAPES = { "&": "&amp;", "'": "&#39;", '"': "&quot;", "<": "&lt;", ">": "&gt;" };

export const el = (id) => document.getElementById(id);
export const show = (id, visible = true) => (el(id).hidden = !visible);

export const escapeHTML = (value) => value.replace(/[&<>'"]/g, (character) => HTML_ESCAPES[character]);
export const fieldValue = (form, name) => String(new FormData(form).get(name) ?? "");

export function element(html) {
    const template = document.createElement("template");
    template.innerHTML = html.trim();

    return template.content.firstElementChild;
}

export const notify = (id, message) => {
    const notice = el(id);

    notice.textContent = message;
    show(id);
    notice.scrollIntoView({ block: "nearest" });
};

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
