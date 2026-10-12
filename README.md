# solana-distance

`solana-distance` is a command-line tool designed to measure the distance in microseconds (µs) to the entire Solana cluster or to specific validators.

Instead of relying on ICMP ECHO (ping), which is often blocked or traffic-shaped by network configurations, it establishes QUIC connections to validators' TPU and measures the round-trip time (RTT) during the connection handshake.


## Installation
1. Clone the repository:
    ```sh
    git clone https://github.com/puffin-validator/solana-distance.git
    ```

2. Change to the project directory:
    ```sh
    cd solana-distance
    ```

3. Compile the tool
    ```sh
    cargo build --release
    ```

The resulting binary will be located in `target/release/solana-distance`.

## Usage
When run without arguments, the tool measures the distance to the entire Solana cluster and displays results after approximately 10 seconds. The most important metric reported is the stake-weighted average distance, which represents network latency and is comparable to half of an RTT as reported by the `ping` command.

```console
$ solana-distance
Simple distance: 31037 ± 6 µs
Simple jitter: 134 µs
Stake-weighted distance: 24463 ± 17 µs
Stake-weighted jitter: 138 µs
Total stake: 438043117 SOL
Connection successful: 227
No successful connection: 2 (0.09% of total stake)
No contact info: 1 (0.05% of total stake)
```

Validators sharing the same TPU address are measured together, so the number of successful connections can be lower than the number of validators.

The reported uncertainty is the standard error of the distance. Jitter is reported separately, when more than one connection attempt is made (see `--count` option): it is the median difference between two latencies measured to the same validator, which makes it insensitive to occasional slow responses.

To measure the distance to one or more specific validators, provide their identity, their name or the address and port of their TPU:
```console
$ solana-distance puffinQSvKFriPbyE5atyx1ptfnyytovbzxybr1jsyy 64.130.57.215:8002 Pebble
Simple distance: 5875 ± 45 µs
Simple jitter: 155 µs
Stake-weighted distance: 5683 ± 79 µs
Stake-weighted jitter: 209 µs
Total stake: 16352822 SOL
Connection successful: 3
```

Add option `--details` to print the distance to each of them.

Names are those published on chain by validators (see `solana validator-info`), limited to nodes of the cluster. Case is ignored, and a name ending with `*` matches names starting with what precedes. The command fails if several validators match a name.

Option `--doublezero` limits the measure to validators connected to Doublezero and is a good way to quantify the impact of being connected to Doublezero.

Option `--no-stake-weighting`, when no validator is specified, measures the distance to all the nodes of the cluster, staked or not. Connections are then made at a slower pace, and results are displayed after approximately 30 seconds.

For a full list of available options, use the `--help` flag.

WARNING: counts higher than 80 are refused, and you should avoid repeating the command during a short period, or you will hit Agave rate limit on QUIC incoming connections (bucket of 80 tokens, per source IP, refilled at a pace of 1 token every 7.5 s).
