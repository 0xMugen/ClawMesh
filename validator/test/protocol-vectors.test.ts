import test from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { ClawMeshValidator } from "../src/index";

function projectRoot(): string {
  return path.resolve(__dirname, "../../..");
}

function loadJson(filePath: string): unknown {
  return JSON.parse(readFileSync(filePath, "utf-8"));
}

function listJsonFiles(dirPath: string): string[] {
  return readdirSync(dirPath)
    .filter((name) => name.endsWith(".json"))
    .map((name) => path.join(dirPath, name))
    .sort();
}

test("validates all protocol spec examples", () => {
  const root = projectRoot();
  const validator = new ClawMeshValidator();
  const files = listJsonFiles(path.join(root, "spec/examples"));

  const result = validator.validateFiles(files);
  assert.equal(result.valid, true, `unexpected errors:\n${result.errors.join("\n")}`);
});

test("validates all schema valid vectors", () => {
  const root = projectRoot();
  const validator = new ClawMeshValidator();
  const files = listJsonFiles(path.join(root, "schemas/examples")).filter((name) => name.endsWith(".valid.json"));

  const result = validator.validateFiles(files);
  assert.equal(result.valid, true, `unexpected errors:\n${result.errors.join("\n")}`);
});

test("rejects known invalid vector", () => {
  const root = projectRoot();
  const validator = new ClawMeshValidator();
  const invalidVector = path.join(root, "schemas/examples/announce.invalid.json");

  const result = validator.validateFile(invalidVector);
  assert.equal(result.valid, false);
  assert.equal(result.errors.length > 0, true);
  assert.equal(result.errors.some((error) => error.includes("envelope")), true);
  assert.equal(result.errors.some((error) => error.includes("payload")), true);
});

test("rejects unknown payload intent even if envelope is otherwise valid", () => {
  const root = projectRoot();
  const validator = new ClawMeshValidator();
  const base = loadJson(path.join(root, "schemas/examples/announce.valid.json")) as Record<string, unknown>;
  const candidate = { ...base, intent: "experimental_intent" };

  const result = validator.validateMessage(candidate, "<inline>");
  assert.equal(result.valid, false);
  assert.equal(result.errors.some((error) => error.includes("no payload schema for intent 'experimental_intent'")), true);
});
