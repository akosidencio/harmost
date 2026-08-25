import { randomUUID } from "node:crypto";
import { beginRender, delay, finishRender } from "@/lib/render";

export async function GET() {
  const sessionId = randomUUID();
  const render = beginRender("/api/private-session");
  try {
    await delay(100);
  } finally {
    finishRender(render);
  }

  return Response.json(
    {
      session_id: sessionId,
      render_id: render.id,
      instance: render.instance,
    },
    {
      headers: {
        "Cache-Control": "private, no-store",
        "Set-Cookie": `session=${sessionId}; HttpOnly; SameSite=Lax; Path=/`,
      },
    },
  );
}
