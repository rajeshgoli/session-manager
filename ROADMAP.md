# Session Manager Roadmap

Session Manager's production server and CLI are implemented in Rust. Current
work is tracked in GitHub issues and in numbered documents under `specs/`.

## Current Direction

- Keep session lifecycle, message delivery, reminders, queue jobs, reviews, and
  restart recovery reliable under concurrent agent workloads.
- Keep the Rust CLI as the single `sm` entry point.
- Provide operator visibility through the terminal dashboard and Android app.
- Preserve durable state compatibility while tightening ownership and
  authentication boundaries.
- Keep live deployment reproducible through `scripts/restart-rust-server.sh`.

## Source of Truth

- `README.md` describes supported behavior and operator setup.
- `CODEBASE_OVERVIEW.md` maps the current Rust implementation.
- `specs/` contains accepted designs and active working documents.
- GitHub issues carry current prioritization and completion state.

The previous Python implementation history remains available in Git rather
than being maintained as a second roadmap in the working tree.
