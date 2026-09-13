import { el, fieldValue, show } from "../dom.js";
import { at, inPounds } from "../format.js";
import { admin, runDraw } from "./actions.js";
import { choices, facts, link, number, row, rows, state } from "./markup.js";
import { attempt, onSubmit } from "./task.js";

const CLOSED = "CLOSED";
const DRAWN = "DRAWN";
const WINNER_STATUSES = ["PENDING", "NOTIFIED", "PAID", "UNCLAIMED"];

const NOT_WITNESSED = "not witnessed";
const NO_DRAW_YET = "not recorded yet";
const NO_WINNERS = "No winners recorded.";
const TOO_EARLY = "The draw opens once ticket sales close and the published draw date is reached.";

const winnerRow = (winner) =>
    row([
        number(String(winner.sequence)),
        number(String(winner.prizeRank)),
        number(inPounds(winner.prizeAmountPence)),
        number(String(winner.ticketNumber)),
        link(`order/${winner.orderId}`, winner.orderId),
        link(`supporter/${winner.entrantId}`, winner.entrantId),
        state(winner.status),
        choices(
            String(winner.sequence),
            `ticket ${winner.ticketNumber}`,
            WINNER_STATUSES.filter((status) => status !== winner.status),
        ),
    ]);

function drawFacts({ draw, ticketsSold, winners }) {
    if (!draw) {
        return facts([
            ["Draw", NO_DRAW_YET],
            ["Tickets sold so far", String(ticketsSold)],
        ]);
    }

    return facts([
        ["Drawn at", at(draw.drawnAt)],
        ["Tickets in the draw", String(draw.ticketsSold)],
        ["Method", draw.method],
        ["Conducted by", draw.conductedBy],
        ["Witnessed by", draw.witnessedBy ?? NOT_WITNESSED],
        ["Winners recorded", String(winners.length)],
    ]);
}

export async function showDraw(raffleId) {
    const detail = await admin("getRaffle", { raffleId });
    const form = el("admin-draw-form");
    const confirm = el("admin-draw-confirm");
    const isRunnable = detail.status === CLOSED || detail.status === DRAWN;

    el("admin-draw-title").textContent = detail.name;
    el("admin-draw-state").textContent = detail.status;
    el("admin-draw-facts").replaceChildren(...drawFacts(detail));
    el("admin-winners-rows").replaceChildren(...rows(detail.winners, winnerRow, NO_WINNERS));
    el("admin-draw-closed").textContent = TOO_EARLY;

    form.reset();
    form.dataset.raffleId = raffleId;
    confirm.pattern = raffleId;

    show("admin-draw-run", isRunnable);
    show("admin-draw-closed", !isRunnable);
}

export function wireDraw(go) {
    const form = el("admin-draw-form");
    const winners = el("admin-winners-rows");

    onSubmit(form, async () => {
        await runDraw({
            conductedBy: fieldValue(form, "conductedBy"),
            raffleId: form.dataset.raffleId,
            witnessedBy: fieldValue(form, "witnessedBy") || undefined,
        });

        await go();
    });

    winners.addEventListener("click", ({ target }) => {
        const { action, value } = target.dataset;

        if (!action) {
            return;
        }

        attempt(target, async () => {
            await admin("setWinnerStatus", { raffleId: form.dataset.raffleId, sequence: Number(value), status: action });

            await go();
        });
    });
}
