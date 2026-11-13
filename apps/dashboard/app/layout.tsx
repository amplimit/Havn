import type { Metadata } from "next";
import "./globals.css";
import { Sidebar } from "@/components/Sidebar";

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
        <div className="flex min-h-screen flex-col md:flex-row">
          <Sidebar />
          <main id="main" tabIndex={-1} className="min-w-0 flex-1 overflow-x-hidden focus:outline-none">
            {children}
          </main>
        </div>
      </body>
    </html>
  );
}
