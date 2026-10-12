mod quic;

use crate::quic::{new_quic_endpoint, socket_addr_to_quic_server_name};
use crate::Error::{ConnectionError, ConnectionFailed, NoContactInfo, NoTPU, NotAStakedNode, OnlyOneSuccessfulConnection};
use clap::Parser;
use quinn::{Endpoint, VarInt};
use rand::Rng;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_types::response::{RpcContactInfo, RpcVoteAccountInfo};
use std::collections::HashMap;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::ops::Add;
use std::path::PathBuf;
use std::time::Duration;
use reqwest::Response;
use serde_json::Value;
use tokio::fs::File;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout};

#[derive(Parser, Debug)]
#[command(version, about = "Measure the distance in µm to the Solana cluster, to Doublezero, or to individual validators")]
struct Args {
    #[arg(help = "Optional list of validator pubkey or TPU ip:port, or a Doublezero network name if option -2 is specified",)]
    destination: Vec<String>,
    #[arg(short, long, help = "Print details for each validator we are connecting to")]
    details: bool,
    #[arg(short, long, help = "Path to a file containing a list of validator pubkey or ip:port")]
    file: Option<PathBuf>,
    #[arg(short='s', long, help = "If specified, disable the stake-weighting of the average distance")]
    no_stake_weighting: bool,
    #[arg(short, long, default_value_t = 5, help = "Number of connection attempts, one attempt is performed every 0,8 secs")]
    count: usize,
    #[arg(short, long, help = "URL of the RPC where cluster info is fetched from", default_value="https://api.mainnet-beta.solana.com")]
    rpc: String,
    #[arg(short='2', long, help = "Measure the distance to the a Doublezero network passed as an optional argument [default: mainnet]")]
    doublezero: bool,
    #[arg(short='p', long, default_value_t = DEFAULT_SOURCE_PORT, help = "Local UDP port connections are made from, 0 to let the system pick a random one")]
    source_port: u16,
}

struct TPU {
    stake: u64,
    join: Option<JoinHandle<Option<(f64, Option<f64>)>>>,
    ids: Vec<String>,
}

#[derive(Eq, Hash, PartialEq)]
enum Error {
    ConnectionFailed,
    OnlyOneSuccessfulConnection,
    ConnectionError,
    NoContactInfo,
    NoTPU,
    NotAStakedNode,
}
struct Errors(HashMap<Error, (u64, u64)>);
impl Errors {
    fn new(&mut self, error: Error, stake: u64) {
        let e = self.0.entry(error).or_insert((0, 0));
        e.0 += 1;
        e.1 += stake;
    }
    fn new_and_print_if(&mut self, error: Error, stake: u64, print: bool) {
        if print { println!("{}", error) }
        self.new(error, stake);
    }
}
impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            ConnectionError => write!(f, "Connection error"),
            ConnectionFailed => write!(f, "No successful connection"),
            OnlyOneSuccessfulConnection => write!(f, "Only one successful connection"),
            NoContactInfo => write!(f, "No contact info"),
            NoTPU => write!(f, "No TPU"),
            NotAStakedNode => write!(f, "Not a staked node"),
        }
    }
}

// The path followed by packets, and so the latency, depends on the source port (multipath routing).
// Using a fixed port by default makes successive measures comparable.
// The value is arbitrary, outside of the default port range of validators and of the range of ephemeral ports.
const DEFAULT_SOURCE_PORT: u16 = 20488;

const LEADER_WINDOW: Duration = Duration::from_millis(4 * 200); // 4 slots
const CONNECTION_TIMEOUT: Duration = Duration::from_millis(1600); // Quinn retransmission fires after 1s, so we need the timeout to be greater than 1s + highest RTT

/// Return latency estimate and its variance, or `None` if no connection succeeded.
/// The variance is `None` if only one connection succeeded.
///
/// Send `count` connection requests, spaced 4 slots apart, to give a good chance that at least one request
/// doesn't arrive when the validator is busy being leader.
/// Add a random temporization if requested.
///
/// We collect latencies and assume they follow a 2-parameter exponential distribution:
/// p(x) = 1/b exp(-(x-a)/b)
/// To be robust against outliers (e.g. the validator being busy), only the r = ceil(n/2) lowest of the
/// n latencies are used (at least 2), the others being treated as censored (type II censoring): all we
/// retain from them is that they are not lower than the highest latency kept.
/// With x(1) <= ... <= x(n) the sorted latencies, the unbiased estimators are:
/// b = (x(1) + ... + x(r) + (n-r)*x(r) - n*x(1)) / (r-1)
/// a = x(1) - b/n
/// var(a) = b^2 / n^2
/// With r = n, these are the usual unbiased estimators for the complete sample:
/// https://www.researchgate.net/publication/233060006_Estimation_in_two-parameter_exponential_distributions
/// Note that `a` can be negative when the latencies are widely spread compared to the lowest one.
async fn latency(endpoint: Endpoint, tpu_quic: SocketAddr, count: usize, temporization: bool) -> Option<(f64, Option<f64>)> {
    let server_name = socket_addr_to_quic_server_name(tpu_quic);
    if temporization {
        let delay= rand::rng().random_range(Duration::ZERO..LEADER_WINDOW);
        sleep(delay).await;
    }
    let mut t = tokio::time::Instant::now();
    let mut lats = Vec::with_capacity(count);
    lats.extend(ping(&endpoint, &server_name, tpu_quic).await);
    for _ in 1..count {
        t = t.add(LEADER_WINDOW);
        sleep_until(t).await;
        lats.extend(ping(&endpoint, &server_name, tpu_quic).await);
    }
    lats.sort_unstable();
    estimate_latency(&lats)
}

/// Estimate the latency and its variance from sorted latencies, see `latency`.
fn estimate_latency(lats: &[u32]) -> Option<(f64, Option<f64>)> {
    let n = lats.len();
    if n < 2 {
        return lats.first().map(|&lat| (lat as f64, None));
    }
    let r = n.div_ceil(2).max(2);
    let lat_min = lats[0] as u64;
    let lat_max = lats[r - 1] as u64;
    let lat_sum: u64 = lats[..r].iter().map(|&lat| lat as u64).sum();
    // Sum of the excesses over the minimum, censored latencies counting as `lat_max`
    let excess = lat_sum + (n - r) as u64 * lat_max - n as u64 * lat_min;
    let b = excess as f64 / (r - 1) as f64;
    let a = lat_min as f64 - b / n as f64;
    Some((a, Some((b * b) / (n * n) as f64)))
}

async fn ping(endpoint: &Endpoint, server_name: &String, tpu_quic: SocketAddr) -> Option<u32> {
    let connecting = endpoint.connect(tpu_quic, server_name).expect("Connection configuration error");
    if let Ok(Ok(connection)) = timeout(CONNECTION_TIMEOUT, connecting).await {
        // With a timeout of 2 s, rtt in µs should never overflow u32.
        let rtt: u32 = connection.rtt().as_micros().try_into().expect("rtt overflow");
        connection.close(VarInt::default(), &[]);
        Some(rtt/2)
    } else {
        None
    }
}

async fn decode_doublezero_info(dz_info: Response) -> Result<Vec<String>, &'static str> {
    let Ok(j) = dz_info.json::<Value>().await else { return Err("Invalid JSON") };
    let Some(j) = j.as_object() else { return Err("Not an object") };
    if j.get("success") != Some(&Value::Bool(true)) { return Err("Failed") };
    let Some(j) = j.get("data") else { return Err("No data") };
    let Some(j) = j.as_object() else { return Err("data is not an object") };
    let Some(j) = j.get("validators") else { return Err("No validators") };
    let Some(j) = j.as_array() else { return Err("validators is not an array") };
    let mut res = Vec::new();
    for v in j {
        let Some(j) = v.as_object() else { return Err("validators is not an array of objects") };
        let Some(j) = j.get("account") else { return Err("validator has no account") };
        res.push(j.as_str().unwrap().to_string());
    }
    if res.is_empty() { return Err("No validators") };
    Ok(res)
}

#[tokio::main]
async fn main() {

    let args = Args::parse();

    let rpc_client = RpcClient::new(args.rpc);

    let mut destination = args.destination;

    if let Some(path) = args.file {
        let file = File::open(path).await.expect("Failed to open specified file");
        let mut lines = io::BufReader::new(file).lines();
        while let Some(line) = lines.next_line().await.expect("Failed to read specified file") {
            destination.push(line);
        }
    }

    if args.doublezero {
        let network = destination.pop().unwrap_or("mainnet".to_string());
        if !destination.is_empty() {
            panic!("Only one Doublezero network name can be specified");
        }
        let url = format!("https://doublezero.xyz/api/dz-validators?network={}", network);
        let dz_info = reqwest::get(&url).await.expect("Cannot send request to Doublezero API");
        destination = decode_doublezero_info(dz_info).await.unwrap_or_else(|e| panic!("Failed to decode Doublezero API response: {}", e));
    }

    let nodes_cnt = destination.len();
    let mut nodes_pk = Vec::new();
    let mut nodes_sa = Vec::new();

    for str in destination.into_iter() {
        match str.parse::<SocketAddr>() {
            Ok(sock_addr) => {
                nodes_sa.push(sock_addr);
            }
            Err(_) => {
                nodes_pk.push(str);
            }
        }
    }

    let mut tpus: HashMap<SocketAddr, TPU> = HashMap::new();
    let mut total_stake = 0;

    let mut errors = Errors(HashMap::new());

    let no_stake_weighting = if nodes_cnt == 1 {
        true
    } else {
        args.no_stake_weighting
    };

    match (nodes_cnt == 0, no_stake_weighting) {

        (true, false) => {
            let rpc_nodes = rpc_client.get_cluster_nodes().expect("Failed to get cluster nodes");
            let rpc_nodes_hash = HashMap::<String, RpcContactInfo>::from_iter(rpc_nodes.into_iter().map(|n| (n.pubkey.clone(), n)));
            let rpc_vote_accounts = rpc_client.get_vote_accounts().expect("Failed to get vote accounts").current;
            for va in rpc_vote_accounts {
                if va.activated_stake != 0 {
                    total_stake += va.activated_stake;
                    if let Some(ci) = rpc_nodes_hash.get(&va.node_pubkey) {
                        if let Some(sock_addr) = ci.tpu_quic {
                            let tpu = tpus.entry(sock_addr).or_insert(TPU {
                                stake: 0,
                                join: None,
                                ids: vec![],
                            });
                            tpu.ids.push(va.node_pubkey.to_string());
                            tpu.stake += va.activated_stake;
                        } else {
                            errors.new(NoTPU, va.activated_stake)
                        }
                    } else {
                        errors.new(NoContactInfo, va.activated_stake)
                    }
                }
            }
        }

        (true, true) => {
            let rpc_nodes = rpc_client.get_cluster_nodes().expect("Failed to get cluster nodes");
            for ci in rpc_nodes {
                if let Some(sock_addr) = ci.tpu_quic {
                    let tpu = tpus.entry(sock_addr).or_insert(TPU {
                        stake: 0,
                        join: None,
                        ids: vec![],
                    });
                    tpu.ids.push(ci.pubkey.to_string());
                } else {
                    errors.new(NoTPU, 0)
                }
            }
        }

        (false, false) => {
            let rpc_nodes = rpc_client.get_cluster_nodes().expect("Failed to get cluster nodes");
            let rpc_vote_accounts = rpc_client.get_vote_accounts().expect("Failed to get vote accounts").current;
            let rpc_pk_vote_accounts = HashMap::<String, &RpcVoteAccountInfo>::from_iter(rpc_vote_accounts.iter().map(|va| (va.node_pubkey.clone(), va)));
            if !nodes_pk.is_empty() {
                let rpc_pk_nodes = HashMap::<String, &RpcContactInfo>::from_iter(rpc_nodes.iter().map(|n| (n.pubkey.clone(), n)));
                for pk in nodes_pk {
                    if let Some(va) = rpc_pk_vote_accounts.get(&pk) {
                        if let Some(ci) = rpc_pk_nodes.get(&pk) {
                            if let Some(sock_addr) = ci.tpu_quic {
                                let tpu = tpus.entry(sock_addr).or_insert(TPU {
                                    stake: 0,
                                    join: None,
                                    ids: vec![],
                                });
                                tpu.ids.push(pk);
                                tpu.stake += va.activated_stake;
                                total_stake += va.activated_stake;
                            } else {
                                errors.new(NoTPU, va.activated_stake)
                            }
                        } else {
                            errors.new(NoContactInfo, va.activated_stake);
                        }
                    } else {
                        errors.new(NotAStakedNode, 0);
                    }
                }
            }
            if !nodes_sa.is_empty() {
                let mut rpc_addr_nodes = HashMap::<SocketAddr, Vec<&RpcContactInfo>>::new();
                for node in &rpc_nodes {
                    if let Some(sock_addr) = node.tpu_quic {
                        rpc_addr_nodes.entry(sock_addr).or_insert(vec![]).push(node);
                    }
                }
                for sock_addr in nodes_sa {
                    let tpu = tpus.entry(sock_addr).or_insert(TPU {
                        stake: 0,
                        join: None,
                        ids: vec![],
                    });
                    for ci in rpc_addr_nodes.get(&sock_addr).unwrap() {
                        if let Some(va) = rpc_pk_vote_accounts.get(&ci.pubkey) {
                            tpu.ids.push(ci.pubkey.clone());
                            tpu.stake += va.activated_stake;
                            total_stake += va.activated_stake;
                        }
                    }
                    if tpu.stake == 0 {
                        errors.new(NotAStakedNode, 0);
                        tpus.remove(&sock_addr);
                    }
                }
            }
        }

        (false, true) => {
            let rpc_nodes = rpc_client.get_cluster_nodes().expect("Failed to get cluster nodes");
            if !nodes_pk.is_empty() {
                let rpc_pk_nodes = HashMap::<String, &RpcContactInfo>::from_iter(rpc_nodes.iter().map(|n| (n.pubkey.clone(), n)));
                for pk in nodes_pk {
                    if let Some(ci) = rpc_pk_nodes.get(&pk) {
                        if let Some(sock_addr) = ci.tpu_quic {
                            let tpu = tpus.entry(sock_addr).or_insert(TPU {
                                stake: 0,
                                join: None,
                                ids: vec![],
                            });
                            tpu.ids.push(pk);
                        } else {
                            errors.new(NoTPU, 0)
                        }
                    } else {
                        errors.new(NoContactInfo, 0);
                    }
                }
            }
            if !nodes_sa.is_empty() {
                let mut rpc_addr_nodes = HashMap::<SocketAddr, Vec<&RpcContactInfo>>::new();
                for node in &rpc_nodes {
                    if let Some(sock_addr) = node.tpu_quic {
                        rpc_addr_nodes.entry(sock_addr).or_insert(vec![]).push(node);
                    }
                }
                for sock_addr in nodes_sa {
                    let tpu = tpus.entry(sock_addr).or_insert(TPU {
                        stake: 0,
                        join: None,
                        ids: vec![],
                    });
                    for ci in rpc_addr_nodes.get(&sock_addr).unwrap() {
                        tpu.ids.push(ci.pubkey.clone());
                    }
                }
            }
        }
    }


    let endpoint = new_quic_endpoint(&Keypair::new(), args.source_port).await;

    let temporization = tpus.len() > 1;
    for (sock_addr, tpu) in &mut tpus {
        tpu.join = Some(tokio::spawn(latency(endpoint.clone(), *sock_addr, args.count, temporization)));
    }

    let mut lat_sum_w = 0.0;
    let mut lat_sum = 0.0;
    let mut lat_cnt = 0;
    let mut lat_stk = 0;

    let mut var_sum_w = 0.0;
    let mut var_sum = 0.0;

    for (sock_addr, tpu) in tpus {
        match tpu.join {
            Some(join) => {
                if args.details {
                    if total_stake > 0 {
                        print!("{:21} {:>9} SOL {:?} ", sock_addr, tpu.stake / 1_000_000_000, tpu.ids);
                    } else {
                        print!("{:21} {:?} ", sock_addr, tpu.ids);
                    }
                }
                match join.await {
                    Ok(None) => {
                        errors.new_and_print_if(ConnectionFailed, tpu.stake, args.details);
                    }
                    Ok(Some((lat, None))) => {
                        // Ignore this measure if args.count > 1 since we won't be able to
                        // compute global variance
                        if args.count == 1 {
                            if total_stake > 0 {
                                lat_sum_w += lat * tpu.stake as f64;
                                lat_stk += tpu.stake;
                            }
                            lat_sum += lat;
                            lat_cnt += 1;
                            if args.details {
                                println!("{:.0} µs", lat);
                            }
                        } else {
                            errors.new_and_print_if(OnlyOneSuccessfulConnection, tpu.stake, args.details);
                        }
                    }
                    Ok(Some((lat, Some(var)))) => {
                        if total_stake > 0 {
                            lat_sum_w += lat * tpu.stake as f64;
                            lat_stk += tpu.stake;
                            var_sum_w += var * tpu.stake as f64;
                        }
                        lat_sum += lat;
                        lat_cnt += 1;
                        var_sum += var;
                        if args.details {
                            // The estimate can be slightly negative, a latency cannot
                            println!("{:.0} ± {:.0} µs", lat.max(0.0), var.sqrt());
                        }
                    }
                    Err(_) => {
                        errors.new_and_print_if(ConnectionError, tpu.stake, args.details);
                    }
                }
            }
            None => {
            }
        }
    }

    if lat_cnt > 0 {
        if args.count > 1 {
            println!("Simple distance: {:.0} ± {:.0} µs", lat_sum / lat_cnt as f64, (var_sum / lat_cnt as f64).sqrt());
        } else {
            println!("Simple distance: {:.0} µs", lat_sum / lat_cnt as f64);
        }
        if total_stake > 0 {
            if args.count > 1 {
                println!("Stake-weighted distance: {:.0} ± {:.0} µs", lat_sum_w / lat_stk as f64, (var_sum_w / lat_stk as f64).sqrt());
            } else {
                println!("Stake-weighted distance: {:.0} µs", lat_sum_w / lat_stk as f64);
            }
            println!("Total stake: {} SOL", lat_stk / 1_000_000_000);
        }
        println!("Connection successful: {}", lat_cnt);
    }

    for (error, (cnt, stk)) in &errors.0 {
        if total_stake > 0 && *error != NotAStakedNode {
            println!("{}: {} ({:.2}% of total stake)", error, cnt, 100.0 * *stk as f64 / (total_stake as f64));
        } else {
            println!("{}: {}", error, cnt);
        }
    }
}