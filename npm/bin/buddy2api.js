#!/usr/bin/env node
// buddy2api npm 入口：spawn 平台二进制（postinstall 下载，见 scripts/install.js）。
const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const binDir = path.join(__dirname, "..", "bin");

// 平台 → 二进制文件名（与 install.js 保持一致）
const FILE = {
  "darwin-arm64": "buddy2api-darwin-arm64",
  "darwin-x64": "buddy2api-darwin-x64",
  "win32-x64": "buddy2api-win32-x64.exe",
  "linux-x64": "buddy2api-linux-x64",
  "linux-arm64": "buddy2api-linux-arm64",
}[`${process.platform}-${process.arch}`];

if (!FILE) {
  console.error(`buddy2api: 不支持平台 ${process.platform}-${process.arch}`);
  process.exit(1);
}

const binPath = path.join(binDir, FILE);
if (!fs.existsSync(binPath)) {
  console.error(
    "buddy2api: 未找到平台二进制，请重新安装（npm install -g buddy2api 触发下载）",
  );
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });
process.exit(result.status ?? 1);
