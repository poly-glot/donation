import { el, fieldValue, show } from "../dom.js";
import { at } from "../format.js";
import { admin } from "./actions.js";
import { link, number, row, rows, state } from "./markup.js";
import { attempt, onSubmit } from "./task.js";

const NO_SUBSCRIBERS = "No subscriptions in this state.";

const page = { cursor: null, status: "" };

const subscriptionRow = (subscription) =>
    row([
        subscription.subscriptionId,
        link(`supporter/${subscription.entrantId}`, subscription.entrantId),
        number(String(subscription.ticketsPerRaffle)),
        at(subscription.eligibleFrom),
        at(subscription.createdAt),
        state(subscription.status),
    ]);

async function appendPage(status) {
    const cursor = page.cursor;
    const listed = await admin("listSubscriptions", { cursor, status });

    if (status !== page.status || cursor !== page.cursor) {
        return;
    }

    page.cursor = listed.cursor ?? null;
    el("admin-subscriptions-rows").append(...(cursor ? listed.subscriptions.map(subscriptionRow) : rows(listed.subscriptions, subscriptionRow, NO_SUBSCRIBERS)));

    show("admin-subscriptions-more", Boolean(page.cursor));
}

async function loadFirstPage(status) {
    Object.assign(page, { cursor: null, status });
    el("admin-subscriptions-rows").replaceChildren();

    await appendPage(status);
}

export async function showSubscriptions(subscriptionId) {
    const filter = el("admin-subscriptions-filter");

    if (subscriptionId) {
        el("admin-cancel-form").elements.namedItem("subscriptionId").value = subscriptionId;
    }

    await loadFirstPage(fieldValue(filter, "status"));
}

export function wireSubscriptions(go) {
    const cancelForm = el("admin-cancel-form");
    const filter = el("admin-subscriptions-filter");
    const more = el("admin-subscriptions-more");

    filter.addEventListener("change", ({ target }) => {
        const status = fieldValue(filter, "status");

        attempt(target, () => loadFirstPage(status));
    });

    more.addEventListener("click", () => attempt(more, () => appendPage(page.status)));

    onSubmit(cancelForm, async () => {
        await admin("cancelSubscription", { subscriptionId: fieldValue(cancelForm, "subscriptionId") });

        await go();
    });
}
