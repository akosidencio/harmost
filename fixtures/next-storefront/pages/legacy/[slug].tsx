import type { GetServerSideProps } from "next";
import {
  beginRender,
  configuredDelay,
  delay,
  finishRender,
} from "@/lib/render";

// The Pages Router half of the fixture.
//
// It is not here for variety. A Pages Router page answers the *same page* in
// two shapes: the document at `/legacy/x`, and — for a client-side navigation —
// a JSON props payload at `/_next/data/<buildId>/legacy/x.json`. That is the
// same class of hazard as the App Router's RSC variant reached by a different
// mechanism, and a proxy that gets it wrong hands a browser expecting HTML a
// blob of JSON.

type LegacyProps = {
  slug: string;
  renderId: string;
  instance: string;
};

export const getServerSideProps: GetServerSideProps<LegacyProps> = async (
  context,
) => {
  const slug = String(context.params?.slug ?? "");
  if (!/^[a-z0-9-]+$/i.test(slug)) {
    return { notFound: true };
  }

  const render = beginRender(`/legacy/${slug}`);
  try {
    await delay(configuredDelay());
  } finally {
    finishRender(render);
  }

  return {
    props: { slug, renderId: render.id, instance: render.instance },
  };
};

export default function LegacyProductPage({
  slug,
  renderId,
  instance,
}: LegacyProps) {
  return (
    <main
      data-origin-instance={instance}
      data-render-id={renderId}
      data-route-kind="pages-router-ssr"
    >
      <p className="eyebrow">Pages Router · getServerSideProps</p>
      <h1>{slug.replaceAll("-", " ")}</h1>
      <p>
        Concurrent requests for this URL should share render {renderId}, and the
        JSON data route for the same page must never be mistaken for it.
      </p>
    </main>
  );
}
