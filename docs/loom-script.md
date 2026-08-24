# Loom script — Knova walkthrough

**Target 5:00.** The spoken text is 741 words — about **5:15** at a normal
140 wpm, or 4:56 if you talk at 150. Take the first one or two cuts listed at
the bottom and it lands comfortably under five minutes. People read slower on
camera than they expect, so plan on making cut #1.

Read the `>` blocks aloud; everything else is a stage direction.

## Before you hit record

Open these tabs **in this order**, so you only ever move right:

| # | Tab | Pre-scroll to |
|---|---|---|
| 1 | `README.md` (rendered on GitHub) | top |
| 2 | `crates/auth-service/src/routes/gateway.rs` | **line 58** |
| 3 | `crates/profile-service/src/routes/profile.rs` | **lines 8–12** (the doc table) |
| 4 | `crates/profile-service/src/store/memory.rs` | **line 190** |
| 5 | `crates/profile-service/src/store/sqlite.rs` | **line 220** |
| 6 | `crates/auth-service/src/users.rs` | **line 56** |
| 7 | terminal at repo root, cleared | `cargo test --workspace` typed, not run |

**Run `cargo test --workspace` once before recording** so the build is warm —
otherwise you sit through a compile on camera.

---

## 0:00 — 0:22 ▸ talking head, or README top

> Hi — my walkthrough of the Knova exercise.
>
> Two services: an authorization service that owns credentials, and a profile
> service behind it. That part is forty-five minutes of work.
>
> So I'll spend the five minutes on what it's really asking: which concurrency
> bug matters, and why matching a status code is only half of hiding whether an
> account exists.

## 0:22 — 0:48 ▸ TAB 2 — `gateway.rs` line 58

> One trust boundary, crossed once.
>
> The client only talks to the auth service. It verifies the JWT, then forwards
> with two headers — an internal token, and the user id out of the verified
> claims.
>
> The profile service never sees a token — it checks that secret in constant
> time and trusts the user id. Anything without it gets a 403. And the gateway
> strips those headers off inbound, so a client can't forge its own.

## 0:48 — 1:30 ▸ TAB 3 — `profile.rs`, the doc table at the top

> Three concurrency hazards here, and only one is hard.
>
> Data races the compiler handles. Torn writes, any lock fixes.
>
> The one that matters is a lost update. Two clients read version four, both
> edit, the second silently erases the first — and both get a 200.
>
> A mutex can't fix that — the window spans two requests, with the user's
> think-time in the middle.
>
> So: optimistic concurrency. The record carries a version, and a stale belief
> gets refused.
>
> The decision is this table. `If-Match` is mandatory. A PUT without one is a
> 428 — refused, not quietly allowed. And `If-Match: *` is rejected, because
> "any version" is exactly the overwrite this prevents.

## 1:30 — 2:05 ▸ TAB 4 `memory.rs:190` → TAB 5 `sqlite.rs:220`

> Here's the enforcement in the in-memory store — compare current version to
> expected, and return Stale rather than writing.
>
> *[switch to sqlite.rs]*
>
> Same guarantee in SQL: `WHERE user_id = ? AND version = ?`. The row count
> decides — one row applied, zero was stale.
>
> Two backends, because a process-local store makes `If-Match: "4"` meaningful
> only to the instance that issued version four. Three replicas, three
> histories, guarantee void. So storage is a trait.

## 2:05 — 2:40 ▸ TAB 7 — run `cargo test --workspace`

> Let me run the tests.
>
> *[run it]*
>
> Fifty-two passing. Twenty of those are one conformance suite against both
> backends — ten tests, two backends — which is what makes the abstraction real
> rather than decorative.
>
> And the check I care about more: delete that compare-and-swap predicate from
> either backend, and three tests fail — the same three, by name, in both.
> That's the evidence the guarantee belongs to the design, not one data
> structure.

## 2:40 — 3:20 ▸ TAB 6 — `users.rs` line 56, then scroll to 121

> Now the security test. This one is structural, not conventional.
>
> Authentication is an enum — Authenticated, or Rejected. An unknown username
> and a wrong password both produce Rejected. The reason never leaves this
> module, so nothing downstream can leak what it was never given.
>
> That's the easy half. The timing half is here — *[scroll to 121]* — on a
> lookup miss we still verify, against a decoy hash. A real Argon2 hash of a
> value no account holds. Same work, always false.
>
> A throttled account gets the identical 401 and still pays for it. Otherwise
> lockout becomes the oracle.

## 3:20 — 4:05 ▸ TAB 1 — README, scroll to Measurement

> On performance — I'll be honest about the order, because that's the
> interesting part.
>
> Round one I tuned the store and the middleware in-process, and got real
> numbers. Then I measured the running system over HTTP — and the gateway hop
> costs sixty percent of throughput. Everything I'd optimised in round one totalled
> four hundred and thirty-nine *nano*seconds. The hop is sixty-seven times
> larger than all of it.
>
> And the biggest single win was a bug I'd written myself. A default log filter
> emitting two lines per request — three hundred and twenty thousand lines
> across a hundred-and-sixty-thousand-request run. Changing one string gave
> back eighteen percent.

## 4:05 — 4:42 ▸ README, "Where AI got it wrong"

> On AI — three things, and the expensive one is mine.
>
> The model reached for a tokio Mutex because "it's async code". Nothing here
> crosses an await, so the std lock is correct and faster.
>
> It put `derive(Debug)` on a struct holding the JWT secret — one debug log
> from leaking a signing key, and nothing warns you.
>
> But the costly one was my own test. Sixty-four writers, assert the version is
> sixty-five. It passed on deliberately broken code — because last-write-wins
> also accepts every write, and also bumps the version.

## 4:42 — 5:00 ▸ talking head

> Which is my answer on agents. The code was the cheap part. The expensive
> parts were deciding lost updates were the real requirement, and noticing my
> test for it proved nothing.
>
> Reviewing generated code line by line doesn't scale. Deciding what would
> prove it wrong does.
>
> It's all in the README. Thanks for watching.

---

## If you're running long, cut in this order

1. **3:20** — drop "Round one, I tuned the store..." and go straight to the hop → **−12s**
2. **4:05** — drop the `derive(Debug)` item → **−14s**
3. **1:30** — don't switch to `sqlite.rs`; say "and the same predicate in SQL" → **−15s**

## Numbers — say these exactly

| Say | Not |
|---|---|
| **52** tests passing | any other total |
| **10 × 2** conformance (20 tests) | "8 × 2" — this was wrong and was corrected today |
| **3** tests fail per mutated backend | — |
| **60%** of throughput lost to the hop | — |
| **439 nanoseconds** / **67×** | — |
| **+18%** from the log filter | — |

All six verified against a live run today.

## Don't claim

- That the SQLite backend is production-ready — it's one connection behind a
  mutex, and that's listed as a known limit in the README.
- That throttle or idempotency state is shared — it's per-process, same flaw
  the profile store had. It's in "still open, honestly" if they ask.
