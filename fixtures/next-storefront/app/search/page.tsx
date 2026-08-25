import { beginRender, configuredDelay, delay, finishRender } from "@/lib/render";

export const dynamic = "force-dynamic";

type SearchPageProps = {
  searchParams: Promise<{ q?: string; page?: string }>;
};

export default async function SearchPage({ searchParams }: SearchPageProps) {
  const { q = "", page = "1" } = await searchParams;
  const render = beginRender(`/search?q=${q}&page=${page}`);
  try {
    await delay(Math.max(100, Math.floor(configuredDelay() / 2)));
  } finally {
    finishRender(render);
  }

  return (
    <main data-origin-instance={render.instance} data-render-id={render.id}>
      <p className="eyebrow">Public dynamic route</p>
      <h1>Search results for “{q || "everything"}”</h1>
      <p>Page {page}; rendered by {render.instance}.</p>
      <div className="grid">
        {["Atlas Runner", "Tempo Trail", "Cloud Sprint"].map((name) => (
          <article key={name}>
            <h2>{name}</h2>
            <p>Deterministic fixture result for cache-key testing.</p>
          </article>
        ))}
      </div>
    </main>
  );
}
