#!/usr/bin/env python3
"""Validate ClawMesh protocol messages against v0.1 schemas."""

from __future__ import annotations

import json
import sys
from pathlib import Path

try:
    from jsonschema import Draft202012Validator, FormatChecker
except ImportError:
    print("error: missing dependency 'jsonschema' (pip install jsonschema)", file=sys.stderr)
    sys.exit(2)

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_ROOT = ROOT / "schemas" / "v0.1"
ENVELOPE_SCHEMA_PATH = SCHEMA_ROOT / "envelope.schema.json"


def _load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def _validate(path: Path, envelope_validator: Draft202012Validator, format_checker: FormatChecker) -> list[str]:
    errors: list[str] = []
    try:
        data = _load_json(path)
    except Exception as exc:  # pragma: no cover - bootstrap script
        return [f"{path}: failed to read JSON: {exc}"]

    for err in envelope_validator.iter_errors(data):
        loc = ".".join(str(x) for x in err.path) or "<root>"
        errors.append(f"{path}: envelope[{loc}] {err.message}")

    intent = data.get("intent")
    if not isinstance(intent, str):
        errors.append(f"{path}: missing/invalid intent for payload dispatch")
        return errors

    payload_schema_path = SCHEMA_ROOT / "messages" / f"{intent}.schema.json"
    if not payload_schema_path.exists():
        errors.append(f"{path}: no payload schema for intent '{intent}'")
        return errors

    payload_schema = _load_json(payload_schema_path)
    payload_validator = Draft202012Validator(payload_schema, format_checker=format_checker)
    payload = data.get("payload", {})

    for err in payload_validator.iter_errors(payload):
        loc = ".".join(str(x) for x in err.path) or "<root>"
        errors.append(f"{path}: payload[{loc}] {err.message}")

    return errors


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print("usage: python validator/validate.py <message.json> [more.json ...]", file=sys.stderr)
        return 2

    envelope_schema = _load_json(ENVELOPE_SCHEMA_PATH)
    format_checker = FormatChecker()
    envelope_validator = Draft202012Validator(envelope_schema, format_checker=format_checker)

    all_errors: list[str] = []
    for raw_path in argv[1:]:
        all_errors.extend(_validate(Path(raw_path), envelope_validator, format_checker))

    if all_errors:
        for line in all_errors:
            print(line, file=sys.stderr)
        return 1

    print(f"OK: {len(argv) - 1} file(s) validated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
