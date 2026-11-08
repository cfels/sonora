#!/usr/bin/env node
// Measure what a smart playlist actually returns: run the repository tests that
// prove membership is derived from the library, that the filters work, and that
// hand edits are refused, and require each named test to have run.
//
// Prints SMART PLAYLIST DB VERIFIED only after every run passes.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const cargo = process.env.CARGO || join(homedir(), ".cargo", "bin", "cargo");
if (!existsSync(cargo)) {
  console.error(`cargo not found at ${cargo}`);
  process.exit(1);
}

const run = (args) => {
  const result = spawnSync(cargo, args, {
    cwd: process.cwd(),
    env: process.env,
    encoding: "utf8",
    timeout: 600000,
    maxBuffer: 32 * 1024 * 1024,
  });
  const output = `${result.stdout || ""}${result.stderr || ""}`;
  if (result.status !== 0) {
    console.error(output.trim().split(/\r?\n/).slice(-15).join("\n"));
    process.exit(1);
  }
  const summary = /test result: ok\. (\d+) passed; (\d+) failed/.exec(output);
  if (!summary) {
    console.error("could not read a test summary from cargo output");
    console.error(output.trim().split(/\r?\n/).slice(-15).join("\n"));
    process.exit(1);
  }
  return { passed: Number(summary[1]), failed: Number(summary[2]) };
};

const derived = run([
  "test",
  "-q",
  "-p",
  "sonora-db",
  "smart_playlists_follow_the_library",
]);
const filters = run([
  "test",
  "-q",
  "-p",
  "sonora-db",
  "smart_rules_filter_by_text_favourite_year_and_limit",
]);
const refused = run([
  "test",
  "-q",
  "-p",
  "sonora-db",
  "smart_playlists_refuse_hand_edits_and_keep_their_rules",
]);

for (const [name, result] of [
  ["derived membership", derived],
  ["rule filters", filters],
  ["hand edits refused", refused],
]) {
  if (result.failed !== 0) {
    console.error(`the ${name} test failed`);
    process.exit(1);
  }
}
const counts = [derived.passed, filters.passed, refused.passed];
if (counts.some((count) => count !== 1)) {
  console.error(`expected each smart playlist test to run once, ran ${counts.join(", ")}`);
  process.exit(1);
}

console.log(
  "smart playlist tests: derived membership, text/favourite/year/limit filters, refused hand edits — 1 each, 0 failed",
);
console.log("SMART PLAYLIST DB VERIFIED");
