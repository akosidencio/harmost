import type { GetServerSideProps } from "next";
import { randomUUID } from "node:crypto";
import { beginRender, delay, finishRender } from "@/lib/render";

// The Set-Cookie barrier, reached through the Pages Router rather than a Route
// Handler. `Set-Cookie` is an absolute never-share rule, and "absolute" has to
// mean on every code path the framework offers, not only the modern one.

type SessionProps = {
  sessionId: string;
  renderId: string;
};

export const getServerSideProps: GetServerSideProps<SessionProps> = async (
  context,
) => {
  const sessionId = randomUUID();
  const render = beginRender("/legacy/session");
  try {
    await delay(100);
  } finally {
    finishRender(render);
  }

  context.res.setHeader(
    "Set-Cookie",
    `legacy_session=${sessionId}; HttpOnly; SameSite=Lax; Path=/`,
  );
  return { props: { sessionId, renderId: render.id } };
};

export default function LegacySessionPage({
  sessionId,
  renderId,
}: SessionProps) {
  return (
    <main data-render-id={renderId} data-session-id={sessionId}>
      <p className="eyebrow">Pages Router · Set-Cookie</p>
      <h1>Legacy session</h1>
      <p>{sessionId}</p>
    </main>
  );
}
