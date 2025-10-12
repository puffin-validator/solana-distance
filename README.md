# solana-distance

`solana-distance` is command-line tool to measure the distance in µs to the whole Solana cluster or to specific validators.

Instead of using ICMP ECHO (ping), which are often blocked or traffic-shaped by network configurations, it opens QUIC connections to validators TPU and measure the RTT during connection handshake.


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
Without any argument, the result is printed, after about 10 seconds. The most important reported value is the stake-weighed average distance. Please note that this is a network latency, and is comparable with half of an RTT, as reported by the `ping` command.  
```console
$ solana-distance
Simple distance: 23019 µs
Connection successful: 948
Stake-weighted distance: 20668 µs
Total stake: 410686394 SOL
No contact info: 1 (0.01% of total stake)
Connection failed: 3 (0.10% of total stake)
```

To measure the distance to one or several specific validators, provide their identity or the adress and port of their TPU:
```console
$ solana-distance puffinQSvKFriPbyE5atyx1ptfnyytovbzxybr1jsyy 64.130.57.131:8009
Simple distance: 539 µs
Connection successful: 2
Stake-weighted distance: 500 µs
Total stake: 13493788 SOL
```

This can be useful to measure the distance to a subset of validators, for instance to assess the impact of DoubleZero:
```shell
solana-distance `doublezero user list|grep -oP 'SolanaValidator: \(\K\w+'`
```

Other options are listed with the `--help` flag