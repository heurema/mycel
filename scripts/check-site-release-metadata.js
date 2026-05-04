#!/usr/bin/env node
"use strict";

const fs = require("fs");

const html = fs.readFileSync("index.html", "utf8");
const jsonLd = html.match(
  /<script type="application\/ld\+json">\n([\s\S]*?)\n<\/script>/,
);

if (!jsonLd) {
  throw new Error("index.html is missing JSON-LD metadata");
}

const metadata = JSON.parse(jsonLd[1]);

if (Object.prototype.hasOwnProperty.call(metadata, "softwareVersion")) {
  throw new Error("site JSON-LD must not hard-code softwareVersion");
}

const forbidden = [
  [/v\d+\.\d+\.\d+/, "hard-coded semantic release label"],
  [/\b\d+\.\d+\.\d+\b/, "hard-coded semantic version"],
  [/\balpha\b/i, "hard-coded alpha lifecycle label"],
  [/boundary core/i, "hard-coded release codename"],
  [/agent contract/i, "hard-coded release codename"],
  [/hero-badge/, "manual hero release badge"],
  [/og-image\.png/, "stale static social image reference"],
  [/summary_large_image/, "large social card without generated image source"],
  [/twitter:image/, "static Twitter image reference"],
];

for (const [pattern, label] of forbidden) {
  if (pattern.test(html)) {
    throw new Error(`index.html contains ${label}: ${pattern}`);
  }
}

const requiredDynamicBadges = [
  "https://img.shields.io/npm/v/mycel-agent",
  "https://img.shields.io/crates/v/mycel",
];

for (const badge of requiredDynamicBadges) {
  if (!html.includes(badge)) {
    throw new Error(`index.html is missing dynamic version badge: ${badge}`);
  }
}

if (fs.existsSync("og-image.png")) {
  throw new Error("remove stale og-image.png or regenerate it without release labels");
}

console.log("site release metadata is dynamic or absent");
