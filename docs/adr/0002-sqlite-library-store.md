# ADR 0002: SQLite for the node's library and local state

Status: accepted (2026-10-19).

## Context

Handoff §4 lists SQLite as the DB. The node is one process per household on a NAS, Pi 5 or mini
PC; the library is tens of thousands of tracks, not millions. Pinakes and Nexus run on Postgres
and Neo4j, but they are shared indexers, not per-node state.

## Decision

`rusqlite` with the bundled SQLite, one file per node, numbered SQL migrations in `pm-library`.
Synchronous calls behind a mutex; the node wraps them in `spawn_blocking`. FTS5 for text search
when the bundled build has it.

## Consequences

- No external service to install; backups are a file copy.
- A per-node Pinakes (§12 "Deployment") would still need Postgres; that is a separate, optional
  deployment and not the node's own store.
