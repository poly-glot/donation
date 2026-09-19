const calendar = new Intl.DateTimeFormat("en-GB", { dateStyle: "long", timeZone: "UTC" });
const gbp = new Intl.NumberFormat("en-GB", { currency: "GBP", style: "currency" });
const stamp = new Intl.DateTimeFormat("en-GB", { dateStyle: "short", timeStyle: "short", timeZone: "UTC" });
const wholeGbp = new Intl.NumberFormat("en-GB", { currency: "GBP", maximumFractionDigits: 0, style: "currency" });

export const day = new Intl.DateTimeFormat("en-GB", { dateStyle: "long" });

export const at = (instant) => stamp.format(new Date(instant));
export const calendarDay = (date) => calendar.format(new Date(date));
export const inFieldValue = (instant) => instant.slice(0, 16);
export const inPounds = (pence) => gbp.format(pence / 100);
export const inWholePounds = (pence) => wholeGbp.format(pence / 100);
export const ticketLabel = ({ number, shard }) => (shard == null ? String(number) : `${shard}-${number}`);
export const toInstant = (fieldValue) => `${fieldValue}:00Z`;
export const toPence = (amount) => Math.round(Number(amount) * 100);
