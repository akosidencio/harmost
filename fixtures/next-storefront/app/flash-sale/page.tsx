import { Suspense } from "react";
import { headers } from "next/headers";
import {
  beginRender,
  configuredDelay,
  delay,
  finishRender,
  type RenderToken,
} from "@/lib/render";

export const dynamic = "force-dynamic";

async function Inventory({ render }: { render: RenderToken }) {
  try {
    await delay(Math.max(900, configuredDelay() * 4));
    return <strong>17 pairs remain</strong>;
  } finally {
    finishRender(render);
  }
}

export default async function FlashSalePage() {
  await headers();
  const render = beginRender("/flash-sale");

  return (
    <main data-origin-instance={render.instance} data-render-id={render.id}>
      <span className="stream-padding" aria-hidden="true">
        {"shell".repeat(600)}
      </span>
      <p className="eyebrow">Streaming public SSR</p>
      <h1>The shell should arrive before inventory.</h1>
      <p>This response is being rendered by {render.instance}.</p>
      <div className="inventory">
        Inventory: {" "}
        <Suspense fallback={<span>checking warehouses…</span>}>
          <Inventory render={render} />
        </Suspense>
      </div>
    </main>
  );
}
