# solana-distance

`solana-distance` is command-line tool designed to measure the distance in microseconds (µs) to the entire Solana cluster or to specific validators.

Instead of relaying on ICMP ECHO (ping), which is often blocked or traffic-shaped by network configurations, it establishes QUIC connections to validators' TPU and measures the round-trip time (RTT) during the connection handshake.


## Installation
1. Clone the repository:
    ```sh
    git clone https://github.com/puffin-validator/solana-distance.git
    ````

2. Change to the projet directory:
    ```sh
    cd solana-distance
    ````

3. Compile the tool
    ```sh
    cargo build --release
    ```

The resulting binary will be located in `target/release/solana-distance`.

## Usage
When run without arguments, the tool measures the distance to the entire Solana cluster and displays results after approximately 10 seconds. The most important metric reported is the stake-weighted average distance, which represents network latency and is comparable to half of the RTT reported by the `ping` command.

```console
$ solana-distance
Simple distance: 23019 µs
Connection successful: 948
Stake-weighted distance: 20668 µs
Total stake: 410686394 SOL
No contact info: 1 (0.01% of total stake)
Connection failed: 3 (0.10% of total stake)
```

To measure the distance to one or more specific validators, provide their identity or the address and port of their TPU:
```console
$ solana-distance puffinQSvKFriPbyE5atyx1ptfnyytovbzxybr1jsyy 64.130.57.131:8009
Simple distance: 539 µs
Connection successful: 2
Stake-weighted distance: 500 µs
Total stake: 13493788 SOL
```

This feature is particularly useful for measuring the distance to a subset of validators, like DoubleZero-connected nodes:
```shell
solana-distance `doublezero user list|grep -oP 'SolanaValidator: \(\K\w+'`
```

For a full list of available options, use the `--help` flag.