import type { Metadata } from "next";
import { Suspense } from "react";
import "./globals.css";
import { PageContent, Sidebar } from "@/components/Sidebar";

export const metadata: Metadata = {
  title: "havn dashboard",
  description:
    "Multi-tenant autonomous agent platform — manage agents, credentials, and chat sessions.",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <body>
        <a href="#main" className="skip-link">
          Skip to content
        </a>
        {/* Pages read their agent or team from the address (`?id=`), which
            a static page only knows in the browser. */}
        <Suspense>
          <div className="flex min-h-screen flex-col md:flex-row">
            <Sidebar />
            <main id="main" tabIndex={-1} className="min-w-0 flex-1 overflow-x-clip focus:outline-none">
              <PageContent>{children}</PageContent>
            </main>
          </div>
        </Suspense>
      </body>
    </html>
  );
}
