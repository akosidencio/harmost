import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",
  poweredByHeader: false,
  deploymentId: process.env.HARMOST_BUILD_ID ?? "next-fixture-v1",
  generateBuildId: async () => process.env.HARMOST_BUILD_ID ?? "next-fixture-v1",
  cacheHandler: require.resolve("./cache-handler.cjs"),
  cacheMaxMemorySize: 0,
  turbopack: {
    root: process.cwd(),
  },
};

export default nextConfig;
