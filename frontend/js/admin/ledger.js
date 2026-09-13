import { el, show } from "../dom.js";
import { at } from "../format.js";
import { admin } from "./actions.js";
import { link, number, row } from "./markup.js";
import { attempt } from "./task.js";

const ledger = { cursor: null, last: 0, loaded: 0, raffleId: "", ticketsSold: 0 };

function coverage({ last, loaded, ticketsSold }) {
    const runs = loaded === 1 ? "1 ticket run" : `${loaded} ticket runs`;

    return `${runs} loaded, covering tickets 1 to ${last} of ${ticketsSold} sold.`;
}

const entryRow = (entry) =>
    row([
        number(String(entry.ticketFrom)),
        number(String(entry.ticketTo)),
        number(String(entry.ticketTo - entry.ticketFrom + 1)),
        link(`order/${entry.orderId}`, entry.orderId),
        link(`supporter/${entry.entrantId}`, entry.entrantId),
        at(entry.allocatedAt),
    ]);

async function appendPage() {
    const raffleId = ledger.raffleId;
    const page = await admin("listEntries", { cursor: ledger.cursor, raffleId });

    if (raffleId !== ledger.raffleId) {
        return;
    }

    ledger.cursor = page.cursor ?? null;
    ledger.last = page.entries.at(-1)?.ticketTo ?? ledger.last;
    ledger.loaded += page.entries.length;

    el("admin-ledger-rows").append(...page.entries.map(entryRow));
    el("admin-ledger-coverage").textContent = coverage(ledger);

    show("admin-ledger-more", Boolean(ledger.cursor));
}

export async function showLedger(raffleId) {
    const raffle = await admin("getRaffle", { raffleId });

    Object.assign(ledger, { cursor: null, last: 0, loaded: 0, raffleId, ticketsSold: raffle.ticketsSold });
    el("admin-ledger-title").textContent = raffle.name;
    el("admin-ledger-rows").replaceChildren();

    await appendPage();
}

export function wireLedger() {
    const more = el("admin-ledger-more");

    more.addEventListener("click", () => attempt(more, appendPage));
}
