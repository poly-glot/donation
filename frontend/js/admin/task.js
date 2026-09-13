import { notify, show } from "../dom.js";

export async function attempt(control, task) {
    control.disabled = true;
    show("admin-error", false);

    try {
        await task();
    } catch (error) {
        notify("admin-error", error.message);
    } finally {
        control.disabled = false;
        control.focus({ preventScroll: true });
    }
}

export function onSubmit(form, task) {
    form.addEventListener("submit", (event) => {
        event.preventDefault();

        attempt(event.submitter, () => task(event.submitter));
    });
}
