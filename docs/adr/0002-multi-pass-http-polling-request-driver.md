# ADR 0002: Multi-pass HTTP polling via a VRL request driver

**Status:** Accepted\
**Date:** 2026-06-16

---

## Context

[ADR 0001](0001-historical-backfill-and-pagination-strategy.md) made `http_polling`
a single-request-per-window source with opt-in `Link`-header pagination. That
model issues **one** request shaped by a `request_vrl` script, optionally follows
`Link: rel="next"` pages, parses the response, and emits each item.

It cannot express **multi-pass** flows where one response drives further
requests:

- **Discovery → detail** — Jenkins (list jobs → per-job builds → per-build
  detail), GitHub REST packages (list → versions → detail). The detail URLs are
  only known after the discovery response.
- **Cursor pagination in the body** — GraphQL APIs return the next cursor inside
  the response body (`pageInfo.endCursor`), not in a `Link` header, so
  `follow_link_header` cannot page them.

These are common for exactly the pull-only systems `http_polling` targets.

---

## Decision

Replace the single-request model with a **VRL request driver**: a bounded
worklist loop. One `driver_vrl` script both seeds and continues the work.

- **Driver contract.** The script is invoked once at **bootstrap**
  (`response = null`) and again for every response whose `route` feeds back. It
  receives `{ metadata, state, request, response }` and must set `.requests` (an
  array; each entry needs a `url`, plus optional `method`, `headers`, `body`,
  `query`, `route`, `parser`). It may set `.state`.
- **Per-request routing.** Each request declares a `route`:
  - `pipeline` (default) — parse the response body and emit `EventSource`(s).
  - `feedback` — hand the response back to the driver to compute more requests.
  - `both` — emit **and** feed back (the GraphQL case: emit `nodes`, feed
    `pageInfo` back).
- **State.** `.state` is carried as an **immutable snapshot** into every request
  the driver emits; when that request's response feeds back, the driver receives
  that snapshot as `.state`. State therefore flows along the request DAG as
  per-edge snapshots — there is no shared mutable state and no merge step.
- This subsumes the old behaviour: a single request is a one-entry `.requests`;
  `Link`-header pagination is a `feedback`/`both` driver pattern.

### Locked trade-offs

- **State is reset per poll.** The driver `state` is in-memory and fresh at the
  start of every poll. Only the `ts_after`/`ts_before` time window is persisted
  across restarts (unchanged from ADR 0001). Cursors that must survive restarts
  are out of scope; they live within one poll's worklist.
- **Concurrency without merge.** Requests are fetched with bounded concurrency
  (`max_concurrency`). Because each request owns an immutable state snapshot,
  concurrent branches never contend for state and need no merge logic. The engine
  overlaps HTTP I/O via a single-task `FuturesUnordered`, so response handling
  (parse, emit, driver re-invocation) stays sequential and the downstream pipe is
  only ever touched from one place.
- **Driver only, no declarative sugar.** No `passes = [...]` layer is shipped; the
  driver is the single concept. A static N-stage pipeline is just a driver that
  branches on `.response == null` / `.response`.
- **Mandatory termination guards.** A driver loop is Turing-flavoured and could
  diverge, so three guards bound every poll: `max_requests` (total requests,
  default 1000), `max_depth` (feedback-chain depth, default 50), and
  `max_concurrency` (in-flight, default 4). Reaching a guard drops remaining work
  and logs; the poll still completes.

### Window advancement

The poll completes when the worklist drains. The window advances unless the
bootstrap driver call failed, or requests were issued but **none** fetched
successfully (a transport/HTTP outage) — then the window is retried next poll. A
2xx response whose body fails to parse still advances the window (re-polling
would not fix a malformed body). This generalises ADR 0001's "don't advance on
failure" to a multi-request poll.

---

## Options Considered

| Option | Description                                                            | Decision                                                  |
| ------ | ---------------------------------------------------------------------- | --------------------------------------------------------- |
| A      | Add an optional second `detail_request_vrl` (max two passes)           | Rejected: cannot express GraphQL cursor-in-body or N hops |
| B      | Declarative `passes = [...]` array compiled to a driver                | Rejected: extra surface; a static unrolling of the driver |
| **C**  | **Single VRL request driver (worklist loop) with per-request routing** | **Accepted**                                              |
| D      | A separate `http_polling_multi` source type                            | Rejected: duplicates window/state/retry logic             |

---

## Consequences

- **Breaking config change.** `request_vrl` → `driver_vrl` (now sets `.requests`,
  not `.url`); `follow_link_header` is removed (express it in the driver). New
  fields: `max_concurrency`, `max_requests`, `max_depth`. Existing single-request
  configs must move their `.url`/`.method`/… into a one-entry `.requests` array.
- `http_polling` now covers discovery→detail and body-cursor pagination in
  addition to everything ADR 0001 enabled.
- Discovery/parent data reaches emitted events via the request's state snapshot,
  surfaced as `metadata.http_polling.state` for a downstream transformer to merge.
- The `Link`-header utility and `follow_link_header` plumbing are removed from the
  source; the equivalent is documented as a driver pattern in the module README.
