import { el, notify, show } from "../dom.js";

export function wireRouter({ nav, subnav }, routes, fallback) {
    const sections = [...new Set(Object.values(routes).map((route) => route.section))];
    const subtabs = [...subnav.querySelectorAll("a")];
    const tabs = [...nav.querySelectorAll(".step-tab")];

    const mark = (anchors, hash) => {
        anchors.forEach((anchor) => anchor.removeAttribute("aria-current"));
        anchors.find((anchor) => anchor.hash === hash)?.setAttribute("aria-current", "page");
    };

    const scopeSubtabs = (name, raffleId) => {
        const scoped = Boolean(raffleId) && subtabs.some((subtab) => subtab.dataset.route === name);

        show("admin-subnav", scoped);

        if (!scoped) {
            mark(subtabs, "");
            return;
        }

        subtabs.forEach((subtab) => (subtab.href = `#${subtab.dataset.route}/${raffleId}`));
        mark(subtabs, `#${name}/${raffleId}`);
    };

    const render = async () => {
        const [name, ...rest] = location.hash.slice(1).split("/");
        const route = Object.hasOwn(routes, name) ? routes[name] : routes[fallback];

        sections.forEach((section) => show(section, section === route.section));
        mark(tabs, `#${route.tab}`);
        scopeSubtabs(name, rest[0]);
        show("admin-error", false);
        el(route.section).focus({ preventScroll: true });

        try {
            await route.show(...rest);
        } catch (error) {
            notify("admin-error", error.message);
        }
    };

    window.addEventListener("hashchange", render);

    return (target) => {
        if (target && location.hash !== `#${target}`) {
            location.hash = target;
            return Promise.resolve();
        }

        return render();
    };
}
