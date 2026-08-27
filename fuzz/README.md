# exponent_core crucible fuzz harness

Stateful invariant fuzzer for the `exponent_core` program, built on
[crucible](https://github.com/asymmetric-research/crucible) and run on FuzzCorp.
It replays randomized instruction sequences in an in-process LiteSVM and checks
protocol invariants (vault solvency, PT/YT supply conservation, per-stream
emission bounds, LP accounting, index monotonicity, …) after every step.

Self-contained crate — it has its own `[workspace]`, so it is **not** part of the
exponent-core build. Crucible is pinned by rev.

```
fuzz/
  crucible/   the harness (42 instruction actions + 12 invariants) and its bundle builder
  mock_sy/    a minimal Standardized-Yield program the harness CPIs into
```

## Why `mock_sy` exists

Almost every interesting path in `exponent_core` — `strip`, `merge`,
`collect_interest`, `collect_emission`, the whole wrapper family — CPIs into a
third-party SY program to read an exchange rate and emission indexes. With none
deployed, those instructions fail before reaching any protocol logic and the
fuzzer explores nothing.

`mock_sy` implements that interface and makes the rate and the emission indexes
**directly settable**, which is the point: several findings only appear when the
SY program reports something the protocol did not anticipate (a depreciating
rate, a rewound cumulative index). A fixed oracle cannot express those states, so
it cannot observe those bugs.

## Build & run

The harness fuzzes a freshly-built program, so build from source first:

```bash
# the SY fixture (must NOT use v1.51 — that linker rejects edition2024)
cargo build-sbf --manifest-path fuzz/mock_sy/Cargo.toml
mkdir -p fuzz/crucible/fixtures
cp fuzz/mock_sy/target/deploy/mock_sy.so fuzz/crucible/fixtures/mock_sy.so

cd fuzz/crucible
cargo test  --release --features invariant_test   # deterministic checks
./build-bundle.sh                                 # program + harness + bundle → ./bundle
```

`build-bundle.sh` builds `exponent_core` itself and refuses to produce a bundle
if either of two things is wrong:

- **any `overwrites values in the frame` diagnostic.** `cargo build-sbf` prints
  those on *stdout while exiting 0*. The default toolchain leaves
  `CollectInterest::try_accounts` 16 bytes over Solana's 4 KB stack frame, and
  the resulting binary silently corrupts deserialized accounts — which
  manufactures protocol "bugs" that do not exist. Hence `--tools-version v1.51`.
- **a dynamically linked harness.** FuzzCorp workers are linux/amd64; a
  glibc-linked binary uploads fine and then dies there with a bare `status 1`.

## CI

`.github/workflows/fuzzcorp.yml` builds the program **from the current source**,
compiles the harness against it, and uploads the bundle to FuzzCorp — so every
change to the program is compiled and fuzzed. Pull requests build the bundle but
do not publish; pushes to `main` publish. Required repo secrets:

- `CRUCIBLE_TOKEN` — read access to the private `asymmetric-research/crucible`
- `FUZZ_API_KEY` — FuzzCorp API key

## Invariants

Twelve properties are documented inline at their check sites in `src/main.rs`
(search `SCOUT:INVARIANT:`). Each carries its own false-positive reasoning and,
where it is gated, an argument for why the gate can actually open.

Already-reported findings are muted via `SCOUT_CHECK_MUTE` in the generated
manifest so campaigns surface only new signal. Muting is announced on stderr at
startup — a silently disabled check is exactly the false negative this harness
exists to avoid.

After any upload, **verify**: a wrong source prefix does not error the campaign,
it silently yields zero coverage.

```bash
fuzz list errors     # must show no SourcesOriginalPath message
fuzz list cover      # lines_found must be > 0 once a cover task has run
```
