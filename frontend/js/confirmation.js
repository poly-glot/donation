import { call } from "./api.js";
import { el, show, sleep } from "./dom.js";

const POLL_INTERVAL_MS = 2_000;
const POLL_TIMEOUT_MS = 60_000;

const RELOAD_ADVICE = "Keep the order number and reload this page in a moment.";
const FAILED_PAYMENT_MSG =
    "Lottery tickets can only be bought with a debit card. " +
    "This payment was declined or made with a credit card, " +
    "so no tickets were issued and any charge has been refunded.";
const PENDING_MSG = `Your tickets are still being allocated. ${RELOAD_ADVICE}`;

async function pollOrderResult(orderId) {
    const deadline = Date.now() + POLL_TIMEOUT_MS;

    while (Date.now() < deadline) {
        const order = await call(`/orders/${orderId}`);

        if (order.tickets) {
            return { status: "SUCCESS", tickets: order.tickets };
        }
        if (order.status === "FAILED") {
            return { status: "FAILED" };
        }

        await sleep(POLL_INTERVAL_MS);
    }

    return { status: "TIMEOUT" };
}

function formatTicketMessage({ from, to }) {
    const label = from === to ? `number is ${from}` : `numbers are ${from} to ${to}`;

    return `Your ticket ${label}. Keep your order number; winners are contacted after the draw.`;
}

async function confirmOrder(orderId) {
    try {
        const result = await pollOrderResult(orderId);

        if (result.status === "SUCCESS") {
            return { message: formatTicketMessage(result.tickets), title: "Thank you. Good luck." };
        }
        if (result.status === "FAILED") {
            return { message: FAILED_PAYMENT_MSG, title: "No tickets issued." };
        }

        return { message: PENDING_MSG };
    } catch (error) {
        return { message: `We could not confirm your order: ${error.message}. ${RELOAD_ADVICE}` };
    }
}

export async function showDone(orderId) {
    el("done-order").textContent = orderId;
    show("done");

    const { message, title } = await confirmOrder(orderId);

    if (title) {
        el("done-title").textContent = title;
    }
    el("done-tickets").textContent = message;
}
