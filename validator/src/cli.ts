#!/usr/bin/env node

import { ClawMeshValidator } from "./index";

function main(argv: string[]): number {
  const filePaths = argv.slice(2);
  if (filePaths.length === 0) {
    console.error("usage: clawmesh-validate <message.json> [more.json ...]");
    return 2;
  }

  const validator = new ClawMeshValidator();
  const result = validator.validateFiles(filePaths);
  if (!result.valid) {
    for (const error of result.errors) {
      console.error(error);
    }
    return 1;
  }

  console.log(`OK: ${filePaths.length} file(s) validated`);
  return 0;
}

process.exitCode = main(process.argv);
