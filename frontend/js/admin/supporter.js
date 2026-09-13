import { el, fieldValue, show } from "../dom.js";
import { at, calendarDay, inPounds } from "../format.js";
import { admin } from "./actions.js";
import { facts, link, number, row, rows, state } from "./markup.js";
import { onSubmit } from "./task.js";

const ERASED_STATE = "ERASED";
const LIVE_STATE = "LIVE";
const NOTHING_RECORDED = "none recorded";
const NO_ORDERS = "No orders.";
const NO_SUBSCRIPTIONS = "No subscriptions.";
const NO_TICKETS = "No tickets allocated.";
const NO_WINS = "No wins.";

const postal = ({ line1, line2, postcode, town }) => [line1, line2, town, postcode].filter(Boolean).join(", ");
const yesNo = (agreed) => (agreed ? "yes" : "no");

const profileFacts = (dossier) =>
    facts([
        ["Supporter", `${dossier.title} ${dossier.firstName} ${dossier.lastName}`.trim()],
        ["Entrant id", dossier.entrantId],
        ["Record", state(dossier.erasedAt ? ERASED_STATE : LIVE_STATE)],
        ["Email", dossier.email],
        ["Telephone", dossier.telephone ?? NOTHING_RECORDED],
        ["Date of birth", calendarDay(dossier.dateOfBirth)],
        ["Address", postal(dossier.address)],
        ["Stripe customer", dossier.stripeCustomerId ?? NOTHING_RECORDED],
        ["First seen", at(dossier.createdAt)],
    ]);

function consentFacts({ consent, giftAidDeclaration }) {
    const declared = giftAidDeclaration
        ? `${yesNo(giftAidDeclaration.isUkTaxpayer)}, ${at(giftAidDeclaration.declaredAt)}, wording ${giftAidDeclaration.wordingVersion}`
        : NOTHING_RECORDED;

    return facts([
        ["Marketing by email", consent ? yesNo(consent.email) : NOTHING_RECORDED],
        ["Marketing by post", consent ? yesNo(consent.post) : NOTHING_RECORDED],
        ["Consent recorded", consent ? `${at(consent.recordedAt)}, wording ${consent.wordingVersion}, via ${consent.source}` : NOTHING_RECORDED],
        ["Gift Aid declared", declared],
    ]);
}

const orderRow = (order) =>
    row([
        link(`order/${order.orderId}`, order.orderId),
        link(`raffle/${order.raffleId}`, order.raffleId),
        state(order.status),
        number(String(order.ticketQuantity)),
        number(inPounds(order.ticketAmountPence)),
        number(inPounds(order.donationPence)),
        number(inPounds(order.totalPence)),
        at(order.createdAt),
    ]);

const entryRow = (entry) =>
    row([
        link(`raffle/${entry.raffleId}`, entry.raffleId),
        number(String(entry.ticketFrom)),
        number(String(entry.ticketTo)),
        number(String(entry.ticketTo - entry.ticketFrom + 1)),
        at(entry.allocatedAt),
    ]);

const subscriptionRow = (subscription) =>
    row([
        link(`subscriptions/${subscription.subscriptionId}`, subscription.subscriptionId),
        number(String(subscription.ticketsPerRaffle)),
        at(subscription.eligibleFrom),
        state(subscription.status),
    ]);

const winnerRow = (winner) =>
    row([
        link(`draw/${winner.raffleId}`, winner.raffleId),
        number(String(winner.sequence)),
        number(inPounds(winner.prizeAmountPence)),
        number(String(winner.ticketNumber)),
        state(winner.status),
    ]);

function renderDossier(dossier) {
    el("admin-dossier-profile").replaceChildren(...profileFacts(dossier));
    el("admin-dossier-consent").replaceChildren(...consentFacts(dossier));
    el("admin-dossier-orders").replaceChildren(...rows(dossier.orders, orderRow, NO_ORDERS));
    el("admin-dossier-entries").replaceChildren(...rows(dossier.entries, entryRow, NO_TICKETS));
    el("admin-dossier-subscriptions").replaceChildren(...rows(dossier.subscriptions, subscriptionRow, NO_SUBSCRIPTIONS));
    el("admin-dossier-wins").replaceChildren(...rows(dossier.winners, winnerRow, NO_WINS));

    const eraseForm = el("admin-erase-form");
    eraseForm.reset();
    eraseForm.dataset.entrantId = dossier.entrantId;
    el("admin-erase-confirm").pattern = dossier.entrantId;

    show("admin-erase", !dossier.erasedAt);
}

export async function showSupporter(entrantId) {
    show("admin-dossier", Boolean(entrantId));

    if (!entrantId) {
        return;
    }

    renderDossier(await admin("getEntrant", { entrantId }));
}

export function wireSupporter(go) {
    const emailForm = el("admin-lookup-email");
    const eraseForm = el("admin-erase-form");
    const orderForm = el("admin-lookup-order");
    const ticketForm = el("admin-lookup-ticket");

    onSubmit(emailForm, async () => {
        const dossier = await admin("findEntrant", { email: fieldValue(emailForm, "email") });

        await go(`supporter/${dossier.entrantId}`);
    });

    onSubmit(orderForm, () => go(`order/${fieldValue(orderForm, "orderId")}`));

    onSubmit(ticketForm, () => go(`ticket/${fieldValue(ticketForm, "raffleId")}/${fieldValue(ticketForm, "ticketNumber")}`));

    onSubmit(eraseForm, async () => {
        await admin("eraseEntrant", { entrantId: eraseForm.dataset.entrantId });

        await go();
    });
}
