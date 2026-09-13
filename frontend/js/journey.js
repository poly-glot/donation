import { el, show } from "./dom.js";

export function wireJourney(form, { entry, start }) {
    const nav = form.querySelector(".steps-nav");
    const tabs = [...form.querySelectorAll(".step-tab")];
    const panels = [...form.querySelectorAll("[data-panel]")];
    let reached = 0;

    const go = (index) => {
        const tab = tabs[index];
        reached = Math.max(reached, index);

        panels.forEach((panel, i) => (panel.hidden = i !== index));
        tabs.forEach((other, i) => {
            other.disabled = i > reached;
            other.removeAttribute("aria-current");
        });
        tab.setAttribute("aria-current", "step");

        el("journey").scrollIntoView();
        nav.scrollTo({ left: tab.offsetLeft - (nav.clientWidth - tab.offsetWidth) / 2 });
        panels[index].focus({ preventScroll: true });
    };

    const isComplete = (container) => {
        const invalid = [...container.querySelectorAll("input")].find((input) => !input.checkValidity());
        invalid?.reportValidity();

        return !invalid;
    };

    const reveal = (field) => {
        const index = panels.findIndex((panel) => panel.contains(field));

        if (index < 0) {
            entry.scrollIntoView();
            return;
        }

        go(index);
    };

    start.addEventListener("click", () => {
        if (!isComplete(entry)) {
            return;
        }

        show("journey");
        go(0);
    });

    tabs.forEach((tab, i) => tab.addEventListener("click", () => go(i)));

    form.addEventListener("click", ({ target }) => {
        if (target.matches("[data-picker]")) {
            entry.scrollIntoView();
            return;
        }

        const index = panels.findIndex((panel) => panel.contains(target));

        if (target.matches("[data-next]") && isComplete(panels[index])) {
            go(index + 1);
        } else if (target.matches("[data-back]")) {
            reveal(index === 0 ? entry : panels[index - 1]);
        }
    });

    return reveal;
}
