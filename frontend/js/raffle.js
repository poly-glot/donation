import { el, element, escapeHTML, show } from "./dom.js";
import { day, inWholePounds } from "./format.js";

function prizeItem({ amountPence, name, quantity }) {
    const quantityMarkup = quantity > 1 ? `<span class="qty">× ${quantity}</span>` : "";

    const html = `
        <li>
            <span class="prize-name">${escapeHTML(name)}${quantityMarkup}</span>
            <span class="amount">${inWholePounds(amountPence)}</span>
        </li>
    `;

    return element(html);
}

export function renderRaffle(raffle) {
    const prizes = raffle.prizes.toSorted((a, b) => a.rank - b.rank);
    const top = prizes[0];
    const count = prizes.reduce((sum, prize) => sum + prize.quantity, 0);

    el("raffle-status").textContent = `Now open · closes ${day.format(new Date(raffle.closesAt))}`;
    el("raffle-title").textContent = top ? `Your chance to win ${inWholePounds(top.amountPence)}.` : raffle.name;
    el("raffle-lede").textContent = `Play the ${raffle.name}. ${inWholePounds(raffle.ticketPricePence)} a ticket, up to ${raffle.maxTicketsPerOrder} per order, ${count} cash prizes.`;
    el("prizes-title").textContent = `${count} cash prizes.`;
    el("prizes").replaceChildren(...prizes.map(prizeItem));
    el("raffle-dates").textContent = `Drawn ${day.format(new Date(raffle.drawAt))}. Results published ${day.format(new Date(raffle.resultsAt))}.`;

    show("order");
}

export function renderClosed(current, next) {
    const opens = next ? ` The next raffle, ${next.name}, opens on ${day.format(new Date(next.opensAt))}.` : "";

    el("closed-copy").textContent = current
        ? `${current.name} closed on ${day.format(new Date(current.closesAt))}. Winners are published two weeks after the draw.${opens}`
        : `There is no raffle open right now.${opens}`;

    show("closed");
}
