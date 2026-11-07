#!/usr/bin/env node
// Prove migration 0006 applies to a brand-new library through the shipped
// binary: run `sonora --history` against an empty data directory, then inspect
// the database it created for the rules column and the applied migration.
//
// Prints SMART MIGRATION VERIFIED only after both are observed.

import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

const cargo = process.env.CARGO || join(homedir(), ".cargo", "bin", "cargo");
if (!existsSync(cargo)) {
  console.error(`cargo not found at ${cargo}`);
  process.exit(1);
}

const dataDir = mkdtempSync(join(tmpdir(), "sonora-gate-"));
let failed = false;
try {
  const app = spawnSync(cargo, ["run", "-q", "-p", "sonora-app", "--", "--history"], {
    cwd: process.cwd(),
    env: { ...process.env, SONORA_DATA_DIR: dataDir },
    encoding: "utf8",
    timeout: 600000,
    maxBuffer: 32 * 1024 * 1024,
  });
  if (app.status !== 0) {
    console.error(`${app.stdout || ""}${app.stderr || ""}`.trim().split(/\r?\n/).slice(-15).join("\n"));
    failed = true;
  }

  const inspect = `
import json, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
columns = [r[1] for r in db.execute("pragma table_info(playlists)")]
versions = [r[0] for r in db.execute("select version from _sqlx_migrations order by version")]
print(json.dumps({"columns": columns, "versions": versions}))
`;
  const probe = spawnSync("python3", ["-c", inspect, join(dataDir, "library.db")], {
    encoding: "utf8",
    timeout: 60000,
  });
  if (probe.status !== 0) {
    console.error(`could not inspect the fresh library: ${probe.stderr || "python3 unavailable"}`);
    process.exit(1);
  }

  const { columns, versions } = JSON.parse(probe.stdout);
  if (!columns.includes("rules")) {
    console.error(`playlists has no rules column; columns: ${columns.join(", ")}`);
    failed = true;
  }
  if (!versions.includes(6)) {
    console.error(`migration 6 is not recorded; applied: ${versions.join(", ")}`);
    failed = true;
  }
  if (failed) process.exit(1);
  console.log(
    `fresh library playlists columns: ${columns.join(", ")}; migrations: ${versions.join(", ")}`,
  );
  console.log("SMART MIGRATION VERIFIED");
} finally {
  rmSync(dataDir, { recursive: true, force: true });
}
