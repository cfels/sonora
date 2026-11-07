#!/usr/bin/env node
// Measure the smart playlist rule document directly: run the core tests that
// cover clamping, year ordering, defaults and the stored JSON form, and require
// that they actually executed.
//
// Prints RULES VERIFIED only after every run passes.

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

const all = run(["test", "-q", "-p", "sonora-core", "playlists::tests"]);
const clamping = run([
  "test",
  "-q",
  "-p",
  "sonora-core",
  "playlists::tests::normalization_clamps_ratings_years_and_limits",
]);
const ordering = run([
  "test",
  "-q",
  "-p",
  "sonora-core",
  "playlists::tests::an_inverted_year_range_is_put_back_in_order",
]);

if (all.failed !== 0 || clamping.failed !== 0 || ordering.failed !== 0) {
  console.error("a rule document test failed");
  process.exit(1);
}
if (all.passed < 5) {
  console.error(`expected at least 5 rule document tests, ran ${all.passed}`);
  process.exit(1);
}
if (clamping.passed !== 1 || ordering.passed !== 1) {
  console.error(
    `expected the clamping and ordering tests to run once each, ran ${clamping.passed} and ${ordering.passed}`,
  );
  process.exit(1);
}

console.log(
  `rule tests: ${all.passed} run, ${all.failed} failed; clamping and year ordering each ran once`,
);
console.log("RULES VERIFIED");
