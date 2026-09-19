import { el } from "../dom.js";
import { at, inPounds, ticketLabel } from "../format.js";
import { admin } from "./actions.js";
import { facts, state } from "./markup.js";

const GIFT_AID_CLAIMED = "claimed on the donation";
const GIFT_AID_UNCLAIMED = "not claimed";
const NOT_CHARGED_YET = "not charged yet";
const NOT_YET = "not yet";
const NO_TICKETS_YET = "No ticket numbers yet: they are allocated when the payment settles.";

function ticketRange(entry) {
    if (!entry) {
        return NO_TICKETS_YET;
    }

    const first = ticketLabel({ number: entry.ticketFrom, shard: entry.shard });
    const last = ticketLabel({ number: entry.ticketTo, shard: entry.shard });

    return entry.ticketFrom === entry.ticketTo ? first : `${first} to ${last}`;
}

function parseTicket(ticket) {
    const [number, shard] = ticket.split("-").reverse();

    return { shard: shard && Number(shard), ticketNumber: Number(number) };
}

function renderOrder(detail) {
    const card = detail.cardLast4 ? `${detail.cardFunding} ending ${detail.cardLast4}` : NOT_CHARGED_YET;

    el("admin-order-facts").replaceChildren(
        ...facts([
            ["Order", detail.orderId],
            ["Raffle", detail.raffleId],
            ["Status", state(detail.status)],
            ["Tickets bought", String(detail.ticketQuantity)],
            ["Ticket money", inPounds(detail.ticketAmountPence)],
            ["Donation", inPounds(detail.donationPence)],
            ["Charged in total", inPounds(detail.totalPence)],
            ["Gift Aid", detail.giftAid ? GIFT_AID_CLAIMED : GIFT_AID_UNCLAIMED],
            ["Card", card],
            ["Paid at", detail.paidAt ? at(detail.paidAt) : NOT_YET],
            ["Ticket numbers", ticketRange(detail.entry)],
        ]),
    );

    const supporter = el("admin-order-supporter");
    supporter.href = `#supporter/${detail.entrantId}`;
    supporter.textContent = detail.entrant ? `${detail.entrant.firstName} ${detail.entrant.lastName}` : detail.entrantId;
}

export async function showOrder(orderId) {
    renderOrder(await admin("getOrder", { orderId }));
}

export async function showTicket(raffleId, ticket) {
    renderOrder(await admin("findTicket", { raffleId, ...parseTicket(ticket) }));
}
