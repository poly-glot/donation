import { post } from "./api.js";
import { el, notify, show } from "./dom.js";
import { inPounds, toPence } from "./format.js";
import { wireJourney } from "./journey.js";

const OTHER = "other";
const UNDER_18_MSG = "You must be 18 or over to enter.";

function readOrder(form, raffle) {
    const data = new FormData(form);
    const picked = (name) => data.get(name);
    const isOther = (name) => picked(name) === OTHER;

    const ticketQuantity = Number(isOther("tickets") ? picked("ticketsOther") : picked("tickets"));
    const donationPence = isOther("donation") ? toPence(picked("donationOther")) : Number(picked("donation"));

    return {
        donationPence,
        entrant: {
            address: {
                country: "GB",
                line1: picked("line1"),
                line2: picked("line2") || undefined,
                postcode: picked("postcode"),
                town: picked("town"),
            },
            dateOfBirth: picked("dateOfBirth"),
            email: picked("email"),
            firstName: picked("firstName"),
            lastName: picked("lastName"),
            telephone: picked("telephone") || undefined,
            title: picked("title"),
        },
        giftAid: picked("giftAid") === "yes",
        marketing: { email: picked("marketingEmail") === "yes", post: picked("marketingPost") === "yes" },
        subscribe: data.has("subscribe"),
        ticketQuantity,
        totalPence: ticketQuantity * raffle.ticketPricePence + donationPence,
    };
}

function eighteenYearsAgo() {
    const date = new Date();
    date.setFullYear(date.getFullYear() - 18);

    return date.toLocaleDateString("en-CA");
}

export function wireForm(form, raffle, { complete, prepare }) {
    const reveal = wireJourney(form, { entry: el("picker"), start: el("start") });
    form.dateOfBirth.max = eighteenYearsAgo();

    const refreshOtherFields = () => {
        const otherTickets = form.tickets.value === OTHER;
        const otherDonation = form.donation.value === OTHER;

        show("tickets-other", otherTickets);
        show("donation-other", otherDonation);
        form.ticketsOther.disabled = !otherTickets;
        form.donationOther.disabled = !otherDonation;
        form.dateOfBirth.setCustomValidity(form.dateOfBirth.validity.rangeOverflow ? UNDER_18_MSG : "");
    };

    const renderSummary = ({ donationPence, ticketQuantity, totalPence }) => {
        el("frame-total").textContent = inPounds(totalPence);
        el("sum-tickets").textContent = `${ticketQuantity} × ${inPounds(raffle.ticketPricePence)}`;
        el("sum-donation").textContent = inPounds(donationPence);
        el("total").textContent = inPounds(totalPence);
    };

    const refresh = () => {
        refreshOtherFields();
        renderSummary(readOrder(form, raffle));
    };

    form.addEventListener("input", refresh);
    refresh();

    form.addEventListener("submit", async (event) => {
        event.preventDefault();

        const invalid = [...form.elements].find((field) => !field.checkValidity());
        if (invalid) {
            reveal(invalid);
            invalid.reportValidity();
            return;
        }

        const { totalPence, ...order } = readOrder(form, raffle);
        el("continue").disabled = true;
        show("order-error", false);

        try {
            const context = prepare();
            const created = await post(`/raffles/${raffle.raffleId}/orders`, order);

            show("order", false);
            window.scrollTo({ behavior: "instant", top: 0 });
            await complete(context, created, order.entrant);
        } catch (error) {
            notify("order-error", error.message);
        } finally {
            el("continue").disabled = false;
        }
    });
}
