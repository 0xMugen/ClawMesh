# Validator (Bootstrap)

Minimal CLI validator for ClawMesh `v0.1` messages.

## Requirements

- Python 3.10+
- `jsonschema` package

Install dependency:

```bash
pip install jsonschema
```

## Usage

```bash
python validator/validate.py schemas/examples/announce.valid.json
```

Validate multiple files:

```bash
python validator/validate.py spec/examples/*.json
```

The validator applies:

1. Envelope validation (`schemas/v0.1/envelope.schema.json`)
2. Payload validation (`schemas/v0.1/messages/<intent>.schema.json`)
