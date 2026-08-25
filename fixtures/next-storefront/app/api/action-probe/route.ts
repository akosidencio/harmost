import { randomUUID } from "node:crypto";

export async function POST() {
  return Response.json(
    {
      mutation_id: randomUUID(),
      instance: process.env.INSTANCE_ID || "unknown",
    },
    { headers: { "Cache-Control": "private, no-store" } },
  );
}
