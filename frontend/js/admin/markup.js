import { element, escapeHTML } from "../dom.js";

const ALERT_STATES = new Set(["CANCELLED", "ERASED", "FAILED", "PAST_DUE", "REFUNDED", "UNCLAIMED"]);

const content = (input) => (typeof input === "string" ? { text: input } : input);

function face({ actions, hash, label, text, value }) {
    if (hash) {
        return `<a class="link" href="#${escapeHTML(hash)}">${escapeHTML(text)}</a>`;
    }
    if (actions) {
        return actions
            .map(
                (name) =>
                    `<button type="button" class="link" aria-label="${escapeHTML(name)} for ${escapeHTML(label)}" data-action="${escapeHTML(name)}" data-value="${escapeHTML(value)}">${escapeHTML(name)}</button>`,
            )
            .join(" ");
    }

    return escapeHTML(text);
}

function cell(input) {
    const inside = content(input);
    const classMarkup = inside.className ? ` class="${inside.className}"` : "";

    return `<td${classMarkup}>${face(inside)}</td>`;
}

function fact([term, input]) {
    const inside = content(input);
    const classNames = inside.className ? `mono ${inside.className}` : "mono";

    return element(`<div class="summary-row"><dt>${escapeHTML(term)}</dt><dd class="${classNames}">${face(inside)}</dd></div>`);
}

const nothingRow = (text) => element(`<tr><td colspan="99">${escapeHTML(text)}</td></tr>`);

export const choices = (value, label, actions) => ({ actions, label, value });
export const facts = (pairs) => pairs.map(fact);
export const link = (hash, text) => ({ hash, text });
export const number = (text) => ({ className: "num", text });
export const row = (cells) => element(`<tr>${cells.map(cell).join("")}</tr>`);
export const rows = (items, build, nothing) => (items.length ? items.map(build) : [nothingRow(nothing)]);
export const state = (status) => ({ className: ALERT_STATES.has(status) ? "state state-alert" : "state", text: status });
