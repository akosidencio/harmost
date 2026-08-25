"use server";

import { randomUUID } from "node:crypto";
import { cookies } from "next/headers";

export async function addToCart(formData: FormData): Promise<void> {
  const product = String(formData.get("product") || "atlas-runner");
  const cookieStore = await cookies();
  const count = Number(cookieStore.get("cart_count")?.value || "0") + 1;
  cookieStore.set("cart_count", String(count), {
    httpOnly: true,
    sameSite: "lax",
    path: "/",
  });

  console.log(
    JSON.stringify({
      event: "server_action",
      action_id: randomUUID(),
      instance: process.env.INSTANCE_ID || "unknown",
      product,
      count,
    }),
  );
}
