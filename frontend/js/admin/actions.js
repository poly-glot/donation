import { invoke } from "../api.js";
import { token } from "./session.js";

export const admin = (action, body = {}) => invoke("/admin", { ...body, action }, token());
export const runDraw = (request) => invoke("/draw", request, token());
