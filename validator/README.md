# Validator (TypeScript/Node)

TypeScript/Node validator package for ClawMesh `v0.1` messages.

## Install

```bash
cd validator
npm install
```

## Build

```bash
npm run build
```

## CLI usage

```bash
npm run build
node dist/src/cli.js schemas/examples/announce.valid.json
```

Validate multiple files:

```bash
node dist/src/cli.js spec/examples/*.json
```

The validator applies:

1. Envelope validation (`schemas/v0.1/envelope.schema.json`)
2. Payload validation (`schemas/v0.1/messages/<intent>.schema.json`)

## Tests

```bash
npm test
```
