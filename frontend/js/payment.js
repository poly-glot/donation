import { config } from "./api.js";
import { showDone } from "./confirmation.js";
import { el, notify, show } from "./dom.js";
import { inPounds } from "./format.js";

const STRIPE_APPEARANCE = {
    theme: "stripe",
    variables: { borderRadius: "2px", colorPrimary: "#111", fontFamily: "system-ui, sans-serif" },
};

function formatSummary({ donationPence, ticketQuantity, totalPence }) {
    const donation = donationPence ? ` and a ${inPounds(donationPence)} donation` : "";

    return `${ticketQuantity} tickets${donation}, ${inPounds(totalPence)} in total.`;
}

function billingDetails(entrant) {
    return {
        address: {
            city: entrant.address.town,
            country: "GB",
            line1: entrant.address.line1,
            line2: entrant.address.line2,
            postal_code: entrant.address.postcode,
            state: "",
        },
        email: entrant.email,
        name: `${entrant.firstName} ${entrant.lastName}`,
    };
}

function mountPaymentElement(stripe, clientSecret) {
    const elements = stripe.elements({ appearance: STRIPE_APPEARANCE, clientSecret });

    elements
        .create("payment", {
            fields: { billingDetails: { address: "never" } },
            wallets: { applePay: "never", googlePay: "never", link: "never" },
        })
        .mount("#payment-element");

    return elements;
}

export function createStripeClient() {
    if (typeof Stripe === "undefined") {
        throw new Error("Stripe.js did not load. Allow js.stripe.com in your browser and try again.");
    }

    return Stripe(config.stripePublishableKey);
}

export function collectPayment(stripe, created, entrant) {
    const pay = el("pay");
    const total = inPounds(created.totalPence);

    el("payment-summary").textContent = formatSummary(created);
    el("payment-order").textContent = created.orderId;
    el("payment-total").textContent = total;
    pay.textContent = `Pay ${total}`;

    show("test-cards", config.stripePublishableKey.startsWith("pk_test_"));
    show("payment");

    const elements = mountPaymentElement(stripe, created.clientSecret);
    const billing = billingDetails(entrant);

    pay.addEventListener("click", async () => {
        pay.disabled = true;
        pay.textContent = "Processing…";
        show("payment-error", false);

        try {
            const { error } = await stripe.confirmPayment({
                confirmParams: {
                    payment_method_data: { billing_details: billing },
                    return_url: `${location.origin}${location.pathname}?order=${created.orderId}`,
                },
                elements,
                redirect: "if_required",
            });

            if (error) {
                throw new Error(error.message);
            }

            show("payment", false);
            await showDone(created.orderId);
        } catch (error) {
            notify("payment-error", error.message);
            pay.disabled = false;
            pay.textContent = `Pay ${total}`;
        }
    });
}
