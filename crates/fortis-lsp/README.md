# fortis-lsp

The fortis wallet's Lightning service provider: [ldk-node](https://github.com/lightningdevkit/ldk-node)
`=0.7.1` in LSPS2-service mode, opening zero-conf just-in-time channels to
fortis clients ("client trusts LSP", bLIP-52). Chain data from Bitcoin Core
over RPC; state in a local data dir (no VSS). Design:
`docs/superpowers/specs/2026-10-30-lightning-design.md`.

```sh
cargo build --release -p fortis-lsp
fortis-lsp --config /etc/fortis/lsp.toml      # default path
```

Config: `lsp.toml.example` (every key, its default and meaning). On start it
prints `node_id <hex>`, `listen <addr>`, `metrics <addr>`,
`onchain_address <addr>` (send liquidity here) and `ready` on stdout;
diagnostics and the low-liquidity warning go to stderr, ldk-node's own log
to `<data_dir>/logs/`. SIGTERM / SIGINT stop the node cleanly (exit 0).

**Node key.** ldk-node's seed file `<data_dir>/keys_seed` (64 random bytes,
created by ldk-node on first start with mode 0400) — the same node id on
every restart. Startup refuses a data dir or seed file that group/others can
access. Losing it loses the node id and the channels' keys: back it up with
the data dir (operations: Task 18's runbook).

**Metrics** (`GET /metrics` on `metrics_listen`, loopback unless
`allow_public_metrics = true`): `fortis_lsp_onchain_sat`, `fortis_lsp_channels`,
`fortis_lsp_inbound_sat`, `fortis_lsp_outbound_sat`,
`fortis_lsp_failed_forwards_total`, `fortis_lsp_pending_htlcs`, plus
`fortis_lsp_peers` and `fortis_lsp_forwards_total`. `GET /info` returns the
node id and the published limits/fees as JSON (never the token).

## Config to ldk-node 0.7.1 `LSPS2ServiceConfig`

| lsp.toml | ldk-node |
|---|---|
| `open_fee_ppm` | `channel_opening_fee_ppm` |
| `open_fee_min_sat` | `min_channel_opening_fee_msat` (× 1000) |
| `min_payment_size_msat` / `max_payment_size_msat` | same names |
| `channel_size_factor` | `channel_over_provisioning_ppm` = (factor − 1) × 10⁶ |
| `token` | `require_token` |
| (fixed) client trusts LSP | `client_trusts_lsp = true` |
| (fixed) | `advertise_service = false`, `min_channel_lifetime = 4320` blocks, `max_client_to_self_delay = 2016` |

**Not expressible in ldk-node 0.7.1:**

- `channel_min_sat` / `channel_max_sat` are validated and published to
  clients (`/info`, the app's limits) but **not enforced on the JIT channel**:
  ldk-node sizes it as (payment − open fee) × `channel_size_factor`, with no
  min/max. With the defaults (500 000 sat max payment, factor 2) a channel
  can reach ~990 000 sat; the binary prints a note at start when
  `max_payment_size_msat × factor > channel_max_sat`. To hold a hard cap,
  lower `max_payment_size_msat` to `channel_max_sat / factor`.
- `fortis_lsp_failed_forwards_total` is always 0: ldk-node handles LDK's
  `HTLCHandlingFailed` internally and emits no event for it.
- `fortis_lsp_pending_htlcs` counts HTLCs awaiting on-chain resolution after
  a channel close (`MaybeTimeoutClaimableHTLC`, `MaybePreimageClaimableHTLC`,
  `ContentiousClaimable`); ldk-node exposes no list of in-channel HTLCs.

## Tests

`cargo test -p fortis-lsp` (config, metrics). The regtest harness LSP for
`fortis-lightning`'s integration tests is this binary
(`crates/fortis-lightning/tests/common/lsp.rs`, smoke test
`tests/lsp_smoke.rs`); build it first with `cargo build -p fortis-lsp`.
