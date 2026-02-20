import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import Ajv2020, { type AnySchemaObject, type ErrorObject, type ValidateFunction } from "ajv/dist/2020";
import addFormats from "ajv-formats";

export type ValidationError = string;

export interface ValidationResult {
  valid: boolean;
  errors: ValidationError[];
}

export interface ValidatorOptions {
  schemaRoot?: string;
}

type Intent = "announce" | "request_match" | "schedule" | "offer";

function defaultSchemaRoot(): string {
  return path.resolve(__dirname, "../../../schemas/v0.1");
}

function loadJson(filePath: string): unknown {
  const raw = readFileSync(filePath, "utf-8");
  return JSON.parse(raw);
}

function toLocation(error: ErrorObject): string {
  const pointer = error.instancePath ?? "";
  const base = pointer.length > 0 ? pointer.slice(1).replaceAll("/", ".") : "<root>";

  if (error.keyword === "required" && typeof error.params === "object" && error.params !== null && "missingProperty" in error.params) {
    const missingProperty = String(error.params.missingProperty);
    return base === "<root>" ? missingProperty : `${base}.${missingProperty}`;
  }

  return base;
}

function formatErrors(source: string, phase: "envelope" | "payload", errors: ErrorObject[] | null | undefined): string[] {
  if (!errors || errors.length === 0) {
    return [];
  }

  return errors.map((error) => `${source}: ${phase}[${toLocation(error)}] ${error.message ?? "validation error"}`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export class ClawMeshValidator {
  private readonly schemaRoot: string;
  private readonly ajv: Ajv2020;
  private readonly envelopeValidator: ValidateFunction;
  private readonly payloadValidators: Map<Intent, ValidateFunction>;

  public constructor(options: ValidatorOptions = {}) {
    this.schemaRoot = options.schemaRoot ?? defaultSchemaRoot();
    this.ajv = new Ajv2020({ allErrors: true, strict: false });
    addFormats(this.ajv);

    const envelopeSchemaPath = path.join(this.schemaRoot, "envelope.schema.json");
    const envelopeSchema = loadJson(envelopeSchemaPath) as AnySchemaObject;
    this.envelopeValidator = this.ajv.compile(envelopeSchema);
    this.payloadValidators = new Map<Intent, ValidateFunction>();
  }

  public validateMessage(message: unknown, source = "<input>"): ValidationResult {
    const errors: string[] = [];
    const envelopeValid = this.envelopeValidator(message);
    if (!envelopeValid) {
      errors.push(...formatErrors(source, "envelope", this.envelopeValidator.errors));
    }

    if (!isRecord(message)) {
      errors.push(`${source}: missing/invalid intent for payload dispatch`);
      return { valid: false, errors };
    }

    const intent = message.intent;
    if (typeof intent !== "string") {
      errors.push(`${source}: missing/invalid intent for payload dispatch`);
      return { valid: false, errors };
    }

    const payloadValidator = this.getPayloadValidator(intent);
    if (!payloadValidator) {
      errors.push(`${source}: no payload schema for intent '${intent}'`);
      return { valid: false, errors };
    }

    const payload = message.payload ?? {};
    const payloadValid = payloadValidator(payload);
    if (!payloadValid) {
      errors.push(...formatErrors(source, "payload", payloadValidator.errors));
    }

    return { valid: errors.length === 0, errors };
  }

  public validateFile(filePath: string): ValidationResult {
    try {
      const message = loadJson(filePath);
      return this.validateMessage(message, filePath);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return { valid: false, errors: [`${filePath}: failed to read JSON: ${message}`] };
    }
  }

  public validateFiles(filePaths: string[]): ValidationResult {
    const allErrors: string[] = [];
    for (const filePath of filePaths) {
      const result = this.validateFile(filePath);
      allErrors.push(...result.errors);
    }

    return { valid: allErrors.length === 0, errors: allErrors };
  }

  private getPayloadValidator(intent: string): ValidateFunction | undefined {
    if (this.payloadValidators.has(intent as Intent)) {
      return this.payloadValidators.get(intent as Intent);
    }

    const payloadSchemaPath = path.join(this.schemaRoot, "messages", `${intent}.schema.json`);
    if (!existsSync(payloadSchemaPath)) {
      return undefined;
    }

    const payloadSchema = loadJson(payloadSchemaPath) as AnySchemaObject;
    const validator = this.ajv.compile(payloadSchema);
    this.payloadValidators.set(intent as Intent, validator);
    return validator;
  }
}
