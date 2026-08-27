import type { AppProps } from "next/app";
import "@/app/globals.css";

// Pages Router entry point. It exists so both routers share one stylesheet; no
// fixture behaviour depends on it.
export default function LegacyApp({ Component, pageProps }: AppProps) {
  return <Component {...pageProps} />;
}
