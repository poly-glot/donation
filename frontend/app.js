import { call, config } from "./js/api.js";
import { showDone } from "./js/confirmation.js";
import { el, notify, show } from "./js/dom.js";
import { wireForm } from "./js/order-form.js";
import { collectPayment, createStripeClient } from "./js/payment.js";
import { renderClosed, renderRaffle } from "./js/raffle.js";

async function boot() {
    if (!config) {
        throw new Error("frontend/config.js is missing. Run scripts/dev.sh to generate it.");
    }

    const returning = new URLSearchParams(location.search).get("order");
    const { current, next } = await call("/raffles/current");

    show("loading", false);
    el("header-raffle").textContent = current?.name ?? "";

    if (returning) {
        await showDone(returning);
    } else if (current?.status === "OPEN") {
        renderRaffle(current);
        wireForm(el("order"), current, { complete: collectPayment, prepare: createStripeClient });
    } else {
        renderClosed(current, next);
    }
}

try {
    await boot();
} catch (error) {
    show("loading", false);
    notify("error", error.message);
}
