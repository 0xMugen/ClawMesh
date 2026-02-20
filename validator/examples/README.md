# Validator Examples

Use sample protocol documents from:

- `schemas/examples/`
- `spec/examples/`

Quick check:

```bash
node validator/dist/src/cli.js schemas/examples/announce.valid.json
```

Validate all current fixtures:

```bash
node validator/dist/src/cli.js schemas/examples/*.json spec/examples/*.json
```
