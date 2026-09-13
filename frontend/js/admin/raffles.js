import { el } from "../dom.js";
import { at, inPounds } from "../format.js";
import { admin } from "./actions.js";
import { link, number, row, rows, state } from "./markup.js";

const NO_RAFFLES = "No raffles yet. Create the first one.";

const raffleRow = (raffle) =>
    row([
        link(`raffle/${raffle.raffleId}`, raffle.raffleId),
        raffle.name,
        state(raffle.status),
        at(raffle.opensAt),
        at(raffle.closesAt),
        at(raffle.drawAt),
        number(String(raffle.ticketsSold)),
        number(inPounds(raffle.ticketRevenuePence)),
        number(inPounds(raffle.donationPence)),
    ]);

export async function showRaffles() {
    const raffles = await admin("listRaffles");

    el("admin-raffles-rows").replaceChildren(...rows(raffles, raffleRow, NO_RAFFLES));
}
