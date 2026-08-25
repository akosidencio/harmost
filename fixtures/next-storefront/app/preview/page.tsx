import { draftMode } from "next/headers";
import { beginRender, delay, finishRender } from "@/lib/render";

export const dynamic = "force-dynamic";

export default async function PreviewPage() {
  const draft = await draftMode();
  const render = beginRender("/preview");
  try {
    await delay(150);
  } finally {
    finishRender(render);
  }

  return (
    <main data-origin-instance={render.instance} data-render-id={render.id}>
      <p className="eyebrow">Draft Mode safety boundary</p>
      <h1>{draft.isEnabled ? "Unpublished winter catalog" : "Published catalog"}</h1>
      <p>
        {draft.isEnabled
          ? "This content is private to the preview session."
          : "Enable Draft Mode through /api/draft to exercise the bypass."}
      </p>
    </main>
  );
}
