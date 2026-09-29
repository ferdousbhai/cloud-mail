import { handleApi } from "./api";
import { handleEmail } from "./ingest";

export default {
  async fetch(req, env): Promise<Response> {
    try {
      return await handleApi(req, env);
    } catch (err) {
      console.error("api error", err);
      return Response.json({ error: "internal error" }, { status: 500 });
    }
  },

  async email(message, env): Promise<void> {
    await handleEmail(message, env);
  },
} satisfies ExportedHandler<Env>;
