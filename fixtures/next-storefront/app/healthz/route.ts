export async function GET() {
  return new Response(null, {
    status: 204,
    headers: {
      "Cache-Control": "no-store",
      "X-Harmost-Build-Id": process.env.HARMOST_BUILD_ID ?? "unknown",
    },
  });
}
