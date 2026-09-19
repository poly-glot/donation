import { el, show } from "../dom.js";
import { at, ticketLabel } from "../format.js";
import { admin } from "./actions.js";
import { link, number, row } from "./markup.js";
import { attempt } from "./task.js";

const ledger = { cursor: null, last: 0, loaded: 0, raffleId: "", shard: 0, shards: 0, ticketsSold: 0 };

const hasMore = ({ cursor, shard, shards }) => Boolean(cursor) || shard < shards;

function coverage({ last, loaded, shards, ticketsSold }) {
    const runs = loaded === 1 ? "1 ticket run" : `${loaded} ticket runs`;

    if (shards) {
        return `${runs} loaded from ${shards} counters, ${ticketsSold} sold in all.`;
    }

    return `${runs} loaded, covering tickets 1 to ${last} of ${ticketsSold} sold.`;
}

const entryRow = (entry) =>
    row([
        number(ticketLabel({ number: entry.ticketFrom, shard: entry.shard })),
        number(ticketLabel({ number: entry.ticketTo, shard: entry.shard })),
        number(String(entry.ticketTo - entry.ticketFrom + 1)),
        link(`order/${entry.orderId}`, entry.orderId),
        link(`supporter/${entry.entrantId}`, entry.entrantId),
        at(entry.allocatedAt),
    ]);

async function appendPage() {
    const raffleId = ledger.raffleId;
    const shard = ledger.shards ? ledger.shard : undefined;
    const page = await admin("listEntries", { cursor: ledger.cursor, raffleId, shard });

    if (raffleId !== ledger.raffleId) {
        return;
    }

    ledger.cursor = page.cursor ?? null;
    ledger.last = page.entries.at(-1)?.ticketTo ?? ledger.last;
    ledger.loaded += page.entries.length;
    if (!ledger.cursor) {
        ledger.shard += 1;
    }

    el("admin-ledger-rows").append(...page.entries.map(entryRow));
    el("admin-ledger-coverage").textContent = coverage(ledger);

    show("admin-ledger-more", hasMore(ledger));
}

export async function showLedger(raffleId) {
    const raffle = await admin("getRaffle", { raffleId });

    Object.assign(ledger, { cursor: null, last: 0, loaded: 0, raffleId, shard: 0, shards: raffle.shards ?? 0, ticketsSold: raffle.ticketsSold });
    el("admin-ledger-title").textContent = raffle.name;
    el("admin-ledger-rows").replaceChildren();

    await appendPage();
}

export function wireLedger() {
    const more = el("admin-ledger-more");

    more.addEventListener("click", () => attempt(more, appendPage));
}
