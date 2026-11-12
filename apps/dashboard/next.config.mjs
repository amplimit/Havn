/**
 * Next.js config. `/api/*` and `/ws/*` are proxied to the gateway by
 * `server.mjs` at runtime, not here, so the gateway address is not baked
 * into the build.
 */
/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // Pin the file-tracing root so Next doesn't pick a parent lockfile and
  // emit the multi-lockfile warning.
  outputFileTracingRoot: import.meta.dirname,
  // `npm run dev` only allows localhost by default; without this, opening
  // the dev server as 127.0.0.1 blocks its hot-reload socket and the page
  // never hydrates.
  allowedDevOrigins: ["127.0.0.1"],
};

export default nextConfig;
