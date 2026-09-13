import { el, fieldValue, show } from "../dom.js";
import { at, inFieldValue, inPounds, toInstant } from "../format.js";
import { admin } from "./actions.js";
import { facts, link, number, row, rows } from "./markup.js";
import { onSubmit } from "./task.js";

const DRAWN = "DRAWN";
const REMOVE = "remove";

const CREATE_LABEL = "Create the raffle";
const UPDATE_LABEL = "Save changes";

const NEW_LEDE = "Dates are UTC and must run opens before closes, closes on or before the draw, and the draw on or before the results. The id may hold letters, digits, a dash and an underscore, and cannot change afterwards.";
const NEW_STATE = "new";
const NEW_TITLE = "A new raffle";
const NOT_YET = "not yet";
const NO_PRIZES = "No prize tiers yet. The draw refuses to run without one.";

const put = (form, name, value) => (form.elements.namedItem(name).value = value);

const counted = (many, noun) => `${many} ${noun}${many === 1 ? "" : "s"}`;

const summary = (raffle) =>
    `${counted(raffle.ticketsSold, "ticket")} sold at ${inPounds(raffle.ticketPricePence)} each, ${counted(raffle.prizes.length, "prize tier")}, ${counted(raffle.winners.length, "winner")} recorded.`;

const prizeRow = (prize) =>
    row([
        link(`raffle/${prize.raffleId}/${prize.rank}`, String(prize.rank)),
        prize.name,
        number(inPounds(prize.amountPence)),
        number(String(prize.quantity)),
    ]);

const counterFacts = (raffle) =>
    facts([
        ["Tickets sold", `${raffle.ticketsSold} of ${raffle.maxTickets}`],
        ["Ticket revenue", inPounds(raffle.ticketRevenuePence)],
        ["Donations", inPounds(raffle.donationPence)],
        ["Ticket price", inPounds(raffle.ticketPricePence)],
        ["Most per order", String(raffle.maxTicketsPerOrder)],
        ["Opened", at(raffle.opensAt)],
        ["Subscriptions charged", raffle.subscriptionsChargedAt ? at(raffle.subscriptionsChargedAt) : NOT_YET],
        ["Drawn", raffle.drawnAt ? at(raffle.drawnAt) : NOT_YET],
    ]);

function fillRaffle(form, raffle) {
    put(form, "raffleId", raffle.raffleId);
    put(form, "name", raffle.name);
    put(form, "ticketPricePence", raffle.ticketPricePence);
    put(form, "maxTicketsPerOrder", raffle.maxTicketsPerOrder);
    put(form, "maxTickets", raffle.maxTickets);
    put(form, "opensAt", inFieldValue(raffle.opensAt));
    put(form, "closesAt", inFieldValue(raffle.closesAt));
    put(form, "drawAt", inFieldValue(raffle.drawAt));
    put(form, "resultsAt", inFieldValue(raffle.resultsAt));
}

function fillPrize(form, raffleId, prize) {
    form.reset();
    form.dataset.raffleId = raffleId;
    put(form, "rank", prize?.rank ?? "");
    put(form, "name", prize?.name ?? "");
    put(form, "amountPence", prize?.amountPence ?? "");
    put(form, "quantity", prize?.quantity ?? 1);
}

function readRaffle(form) {
    return {
        closesAt: toInstant(fieldValue(form, "closesAt")),
        drawAt: toInstant(fieldValue(form, "drawAt")),
        maxTickets: Number(fieldValue(form, "maxTickets")),
        maxTicketsPerOrder: Number(fieldValue(form, "maxTicketsPerOrder")),
        name: fieldValue(form, "name"),
        opensAt: toInstant(fieldValue(form, "opensAt")),
        raffleId: fieldValue(form, "raffleId"),
        resultsAt: toInstant(fieldValue(form, "resultsAt")),
        ticketPricePence: Number(fieldValue(form, "ticketPricePence")),
    };
}

function readPrize(form) {
    return {
        amountPence: Number(fieldValue(form, "amountPence")),
        name: fieldValue(form, "name"),
        quantity: Number(fieldValue(form, "quantity")),
        raffleId: form.dataset.raffleId,
        rank: Number(fieldValue(form, "rank")),
    };
}

function renderNew(form) {
    form.reset();
    form.dataset.action = "createRaffle";
    form.elements.namedItem("raffleId").readOnly = false;

    el("admin-raffle-submit").textContent = CREATE_LABEL;
    el("admin-raffle-title").textContent = NEW_TITLE;
    el("admin-raffle-state").textContent = NEW_STATE;
    el("admin-raffle-lede").textContent = NEW_LEDE;
}

function renderRecord(form, detail, rank) {
    form.dataset.action = "updateRaffle";
    form.elements.namedItem("raffleId").readOnly = true;
    fillRaffle(form, detail);
    fillPrize(el("admin-prize-form"), detail.raffleId, detail.prizes.find((prize) => prize.rank === Number(rank)));

    el("admin-raffle-submit").textContent = UPDATE_LABEL;
    el("admin-raffle-title").textContent = detail.name;
    el("admin-raffle-state").textContent = detail.status;
    el("admin-raffle-lede").textContent = summary(detail);
    el("admin-raffle-counters").replaceChildren(...counterFacts(detail));
    el("admin-prizes-rows").replaceChildren(...rows(detail.prizes, prizeRow, NO_PRIZES));

    show("admin-prize-form", detail.status !== DRAWN);
}

export async function showRaffle(raffleId, rank) {
    const isNew = !raffleId;
    const form = el("admin-raffle-form");

    show("admin-raffle-record", !isNew);
    show("admin-raffle-offer", !isNew);

    if (isNew) {
        renderNew(form);
        return;
    }

    renderRecord(form, await admin("getRaffle", { raffleId }), rank);
}

export function wireRaffle(go) {
    const form = el("admin-raffle-form");
    const prizeForm = el("admin-prize-form");

    onSubmit(form, async () => {
        const input = readRaffle(form);

        await admin(form.dataset.action, input);

        await go(`raffle/${input.raffleId}`);
    });

    onSubmit(prizeForm, async (submitter) => {
        const removing = submitter.value === REMOVE;

        if (removing && !prizeForm.elements.namedItem("rank").reportValidity()) {
            return;
        }

        await admin(removing ? "removePrize" : "putPrize", readPrize(prizeForm));

        await go();
    });
}
