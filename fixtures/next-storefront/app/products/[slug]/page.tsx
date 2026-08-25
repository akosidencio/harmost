import Image from "next/image";
import { notFound } from "next/navigation";
import {
  beginRender,
  configuredDelay,
  delay,
  finishRender,
} from "@/lib/render";

export const dynamic = "force-dynamic";

type ProductPageProps = {
  params: Promise<{ slug: string }>;
};

export default async function ProductPage({ params }: ProductPageProps) {
  const { slug } = await params;
  if (!/^[a-z0-9-]+$/i.test(slug)) {
    notFound();
  }

  const render = beginRender(`/products/${slug}`);
  try {
    await delay(configuredDelay());
  } finally {
    finishRender(render);
  }

  return (
    <main
      data-origin-instance={render.instance}
      data-render-id={render.id}
      data-route-kind="public-ssr"
    >
      <section className="product">
        <Image
          src="/product.svg"
          width={560}
          height={420}
          sizes="(max-width: 800px) 100vw, 50vw"
          alt="Abstract running shoe"
        />
        <div>
          <p className="eyebrow">Rendered by {render.instance}</p>
          <h1>{slug.replaceAll("-", " ")}</h1>
          <p className="price">$128.00</p>
          <p>
            This public page intentionally pauses before returning. Concurrent
            requests for the same URL should share render {render.id}.
          </p>
        </div>
      </section>
    </main>
  );
}
