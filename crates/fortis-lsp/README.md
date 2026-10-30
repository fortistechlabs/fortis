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
| `min_payment_size_msat` / `max_payment_size_msat`, narrowed by `channel_min_sat` / `channel_max_sat` (below) | `min_payment_size_msat` / `max_payment_size_msat` |
| `channel_size_factor` | `channel_over_provisioning_ppm` = (factor − 1) × 10⁶ |
| `token` | `require_token` |
| (fixed) client trusts LSP | `client_trusts_lsp = true` |
| (fixed) | `advertise_service = false`, `min_channel_lifetime = 4320` blocks, `max_client_to_self_delay = 2016` |

**Channel size bounds.** ldk-node 0.7.1 has no channel size setting: it
sizes a JIT channel from the payment P as (P − fee) × factor, with
fee = max(⌈P · `open_fee_ppm` / 10⁶⌉, `open_fee_min_sat`) and the factor
applied as integer over-provisioning ppm. `config::effective_payment_bounds`
narrows the configured payment bounds so that every channel lands in
`channel_min_sat..=channel_max_sat`, using the same integer maths:

- max payment = ⌊`channel_max_sat` · 1000 / factor⌋ msat (a payment never
  yields more than P × factor), at most `max_payment_size_msat`;
- min payment = the smallest P (binary search; the channel size grows with
  P) whose channel, fee included, reaches `channel_min_sat`, at least
  `min_payment_size_msat`.

With the defaults (1 %, 1 000 sat floor, factor 2, 100 000–500 000 sat) that
is 51 000–250 000 sat. These effective bounds go to ldk-node, `GET /info`
and the startup line on stderr. A config whose bounds leave no payment size
is refused at load; `channel_size_factor` is capped at 10 and
`max_payment_size_msat` at 21 000 000 BTC (ldk-node's own arithmetic is
unchecked `u64`).

**Spec deviation: no routing fees on JIT channels.** ldk-node 0.7.1 sets
`forwarding_fee_base_msat` and `forwarding_fee_proportional_millionths` to 0
on every JIT channel it opens (`liquidity.rs`, `OpenChannel`), so on those
channels the LSP earns only the channel-opening fee; the spec's "normal
routing fees otherwise" is not available for them.

**Not available in ldk-node 0.7.1:**

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
