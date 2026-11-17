# tipper

Tops up a list of accounts to a target balance from one funder key. Every `interval_secs` it
reads each account's balance and sends the difference when it is below `target`. That is all it
does.

## Configuration

One YAML file, see [`config.example.yaml`](config.example.yaml):

```yaml
rpc_url: https://ethereum-sepolia-rpc.publicnode.com   # or RPC_URL in the environment
interval_secs: 600
max_topup: 2            # ETH; a single top-up never sends more than this
accounts:
  - name: batcher
    address: "0x…"
    target: 1           # ETH; topped up to this when below
```

The funder's private key is read from `FUNDER_KEY` only, never from the file.

## Running

```bash
cargo run --release -- --config config.yaml --dry-run --once   # read balances, say what it would send
FUNDER_KEY=0x… cargo run --release -- --config config.yaml       # top up every interval_secs
```

Flags: `--once` does one pass and exits; `--dry-run` needs no key and sends nothing. Logging is
`RUST_LOG`, default `info`.

## Docker

```bash
docker build -t tipper .
docker run --rm -e FUNDER_KEY=0x… -v $PWD/config.yaml:/etc/tipper/config.yaml:ro tipper
```

The image runs as an unprivileged user and reads `/etc/tipper/config.yaml` by default.

## Metrics

Prometheus on `--metrics-addr` (default `0.0.0.0:9090`, also `METRICS_ADDR`), at `/metrics`:

| Metric | Labels | Meaning |
| --- | --- | --- |
| `tipper_balance_ether` | `account` | the balance read on the last pass |
| `tipper_funder_balance_ether` | | the funder's balance on the last pass |
| `tipper_topups_total` | `account` | successful top-ups |
| `tipper_topup_failures_total` | `account` | failed attempts |
| `tipper_topped_up_ether` | `account` | ETH sent since start, a gauge that only grows |

## Safety

- `max_topup` bounds every single transfer, so a wrong target cannot drain the funder.
- Use a dedicated funder account holding only what the accounts need, never an owner key.
- A failed top-up is logged and the next account is tried; the loop never stops on an error.
