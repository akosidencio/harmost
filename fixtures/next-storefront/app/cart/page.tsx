import { cookies } from "next/headers";
import { addToCart } from "./actions";

export const dynamic = "force-dynamic";

export default async function CartPage() {
  const cookieStore = await cookies();
  const count = Number(cookieStore.get("cart_count")?.value || "0");

  return (
    <main>
      <p className="eyebrow">Real Server Action</p>
      <h1>Cart</h1>
      <p>Items: {count}</p>
      <form action={addToCart}>
        <input type="hidden" name="product" value="atlas-runner" />
        <button type="submit">Add Atlas Runner</button>
      </form>
      <p>Every submission must bypass cache reuse and request coalescing.</p>
    </main>
  );
}
