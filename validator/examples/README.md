# Validator Examples

Use sample protocol documents from:

- `schemas/examples/`
- `spec/examples/`

Quick check:

```bash
python validator/validate.py schemas/examples/announce.valid.json
```

Validate all current fixtures:

```bash
python validator/validate.py schemas/examples/*.json spec/examples/*.json
```
