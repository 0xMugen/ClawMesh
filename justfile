default:
  @just --list

shell:
  nix develop

fmt:
  cargo fmt --all

check:
  cargo clippy --all-targets

test:
  cargo test --all

test-validator:
  cd validator && npm run test
