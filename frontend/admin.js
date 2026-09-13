import { showDraw, wireDraw } from "./js/admin/draw.js";
import { showLedger, wireLedger } from "./js/admin/ledger.js";
import { showOrder, showTicket } from "./js/admin/order.js";
import { showRaffle, wireRaffle } from "./js/admin/raffle.js";
import { showRaffles } from "./js/admin/raffles.js";
import { wireRouter } from "./js/admin/router.js";
import { restore, signIn, signOut } from "./js/admin/session.js";
import { showSubscriptions, wireSubscriptions } from "./js/admin/subscriptions.js";
import { showSupporter, wireSupporter } from "./js/admin/supporter.js";
import { attempt } from "./js/admin/task.js";
import { el, notify, show } from "./js/dom.js";

const FALLBACK = "supporter";

const ROUTES = {
    draw: { section: "admin-draw", show: showDraw, tab: "raffles" },
    ledger: { section: "admin-ledger", show: showLedger, tab: "raffles" },
    order: { section: "admin-order", show: showOrder, tab: "supporter" },
    raffle: { section: "admin-raffle", show: showRaffle, tab: "raffles" },
    raffles: { section: "admin-raffles", show: showRaffles, tab: "raffles" },
    subscriptions: { section: "admin-subscriptions", show: showSubscriptions, tab: "subscriptions" },
    supporter: { section: "admin-supporter", show: showSupporter, tab: "supporter" },
    ticket: { section: "admin-order", show: showTicket, tab: "supporter" },
};

async function boot() {
    const start = el("admin-signin-start");

    start.addEventListener("click", () => attempt(start, signIn));
    el("admin-signout").addEventListener("click", signOut);

    const signedIn = await restore();
    show("admin-loading", false);

    if (!signedIn) {
        show("admin-signin");
        return;
    }

    const go = wireRouter({ nav: el("admin-nav"), subnav: el("admin-subnav") }, ROUTES, FALLBACK);

    wireDraw(go);
    wireLedger();
    wireRaffle(go);
    wireSubscriptions(go);
    wireSupporter(go);

    show("admin-console");
    show("admin-signout");

    await go();
}

try {
    await boot();
} catch (error) {
    show("admin-loading", false);
    notify("admin-error", error.message);
}
