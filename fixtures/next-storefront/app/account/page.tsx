import { cookies } from "next/headers";
import { beginRender, delay, finishRender } from "@/lib/render";

export const dynamic = "force-dynamic";

export default async function AccountPage() {
  const cookieStore = await cookies();
  const session = cookieStore.get("session")?.value ?? "anonymous";
  const render = beginRender("/account");
  try {
    await delay(100);
  } finally {
    finishRender(render);
  }

  return (
    <main data-origin-instance={render.instance} data-render-id={render.id}>
      <p className="eyebrow">Private dynamic route</p>
      <h1>Account</h1>
      <p data-session={session}>Session: {session}</p>
      <p>This response must never be cached or coalesced.</p>
    </main>
  );
}
