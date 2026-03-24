#!/usr/bin/env node
"use strict";

const { execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const https = require("https");
const http = require("http");

const VERSION = require("./package.json").version;
const REPO = "heurema/mycel";
const BIN_DIR = path.join(__dirname, "bin");
const BIN_PATH = path.join(BIN_DIR, "mycel-bin");

const PLATFORM_MAP = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "linux-x64": "x86_64-unknown-linux-gnu",
};

function getTarget() {
  const key = `${process.platform}-${process.arch}`;
  const target = PLATFORM_MAP[key];
  if (!target) {
    console.error(`Unsupported platform: ${key}`);
    console.error(`Supported: ${Object.keys(PLATFORM_MAP).join(", ")}`);
    console.error("Install from source: cargo install mycel");
    process.exit(1);
  }
  return target;
}

function downloadUrl(target) {
  return `https://github.com/${REPO}/releases/download/v${VERSION}/mycel-${target}.tar.gz`;
}

function follow(url) {
  return new Promise((resolve, reject) => {
    const get = url.startsWith("https:") ? https.get : http.get;
    get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        follow(res.headers.location).then(resolve, reject);
        return;
      }
      if (res.statusCode !== 200) {
        reject(new Error(`Download failed: HTTP ${res.statusCode} from ${url}`));
        return;
      }
      resolve(res);
    }).on("error", reject);
  });
}

function extractTarGz(stream, destDir) {
  const { createGunzip } = require("zlib");
  const gunzip = createGunzip();
  const chunks = [];

  return new Promise((resolve, reject) => {
    const decompressed = stream.pipe(gunzip);
    decompressed.on("data", (chunk) => chunks.push(chunk));
    decompressed.on("error", reject);
    decompressed.on("end", () => {
      const buf = Buffer.concat(chunks);

      // Minimal tar parser — extract first regular file
      let offset = 0;
      while (offset < buf.length) {
        if (offset + 512 > buf.length) break;
        const header = buf.subarray(offset, offset + 512);

        // End-of-archive (two zero blocks)
        if (header.every((b) => b === 0)) break;

        const name = header.subarray(0, 100).toString("utf8").replace(/\0/g, "");
        const sizeOctal = header.subarray(124, 136).toString("utf8").replace(/\0/g, "").trim();
        const size = parseInt(sizeOctal, 8) || 0;

        offset += 512; // skip header

        if (name && size > 0 && !name.endsWith("/")) {
          const data = buf.subarray(offset, offset + size);
          const dest = path.join(destDir, "mycel-bin");
          fs.writeFileSync(dest, data, { mode: 0o755 });
        }

        // Advance past data blocks (rounded up to 512)
        offset += Math.ceil(size / 512) * 512;
      }

      resolve();
    });
  });
}

async function main() {
  // Skip in CI if MYCEL_SKIP_INSTALL is set
  if (process.env.MYCEL_SKIP_INSTALL) {
    console.log("mycel: skipping binary download (MYCEL_SKIP_INSTALL)");
    return;
  }

  // Already installed?
  if (fs.existsSync(BIN_PATH)) {
    try {
      const out = execFileSync(BIN_PATH, ["--version"], { encoding: "utf8" }).trim();
      if (out.includes(VERSION)) {
        return; // correct version already present
      }
    } catch {}
  }

  const target = getTarget();
  const url = downloadUrl(target);

  console.log(`mycel: downloading v${VERSION} for ${target}...`);

  const res = await follow(url);
  fs.mkdirSync(BIN_DIR, { recursive: true });
  await extractTarGz(res, BIN_DIR);

  if (!fs.existsSync(BIN_PATH)) {
    console.error("mycel: binary not found after extraction");
    process.exit(1);
  }

  fs.chmodSync(BIN_PATH, 0o755);
  console.log(`mycel: installed v${VERSION}`);
}

main().catch((err) => {
  console.error(`mycel: installation failed — ${err.message}`);
  console.error("Install from source: cargo install mycel");
  process.exit(1);
});
