/**
 * Next.js config — proxies REST + WS to the gateway during dev so the browser
 * doesn't have to hop origins (gateway listens on 127.0.0.1:8080 by default).
 * Production deployments configure the reverse proxy at the edge instead.
 */
/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // Pin the file-tracing root so Next doesn't pick a parent lockfile and
  // emit the multi-lockfile warning.
  outputFileTracingRoot: import.meta.dirname,
  async rewrites() {
    const gateway = process.env.HAVN_GATEWAY_URL ?? "http://127.0.0.1:8080";
    return [
      { source: "/api/:path*", destination: `${gateway}/:path*` },
    ];
  },
};

export default nextConfig;
