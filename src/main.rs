mod quic;

use crate::quic::{new_quic_endpoint, socket_addr_to_quic_server_name};
use clap::Parser;
use quinn::{Endpoint, VarInt};
use rand::Rng;
use solana_keypair::{Address, Keypair};
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_types::request::RpcRequest;
use solana_rpc_client_types::response::RpcContactInfo;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::ops::Add;
use std::path::PathBuf;
use std::time::Duration;
use reqwest::Response;
use serde_json::{json, Value};
use tokio::fs::File;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Instant};

#[derive(Parser, Debug)]
#[command(version, about = "Measure the distance in µm to the Solana cluster, to Doublezero, or to individual validators")]
struct Args {
    #[arg(help = "Optional list of validator pubkey, name or TPU ip:port, or a Doublezero network name if option -2 is specified. A name ending with * matches names starting with what precedes",)]
    destination: Vec<String>,
    #[arg(short, long, help = "Print details for each validator we are connecting to")]
    details: bool,
    #[arg(short, long, conflicts_with = "doublezero", help = "Path to a file containing a list of validator pubkey, name or ip:port, one per line")]
    file: Option<PathBuf>,
    #[arg(short='s', long, help = "If specified, disable the stake-weighting of the average distance")]
    no_stake_weighting: bool,
    #[arg(short, long, default_value_t = 5, value_parser = parse_count, help = "Number of connection attempts, between 1 and 80, one attempt is performed every 0,8 secs, or more when there are more than 400 TPUs to connect to")]
    count: usize,
    #[arg(short, long, help = "URL of the RPC where cluster info is fetched from", default_value="https://api.mainnet-beta.solana.com")]
    rpc: String,
    #[arg(short='2', long, help = "Measure the distance to the a Doublezero network passed as an optional argument [default: mainnet]")]
    doublezero: bool,
    #[arg(short='p', long, default_value_t = DEFAULT_SOURCE_PORT, help = "Local UDP port connections are made from, 0 to let the system pick a random one")]
    source_port: u16,
}

// Agave limits the rate of incoming QUIC connections per source IP with a bucket of 80 tokens,
// refilled at a pace of 1 token every 7.5 s.
const MAX_COUNT: usize = 80;

fn parse_count(s: &str) -> Result<usize, String> {
    match s.parse() {
        Ok(count) if (1..=MAX_COUNT).contains(&count) => Ok(count),
        _ => Err(format!("count must be between 1 and {}", MAX_COUNT)),
    }
}

#[derive(Default)]
struct TPU {
    stake: u64,
    join: Option<JoinHandle<Vec<u32>>>,
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
    UnknownAddress,
    UnknownName,
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
            Error::ConnectionError => write!(f, "Connection error"),
            Error::ConnectionFailed => write!(f, "No successful connection"),
            Error::OnlyOneSuccessfulConnection => write!(f, "Only one successful connection"),
            Error::NoContactInfo => write!(f, "No contact info"),
            Error::NoTPU => write!(f, "No TPU"),
            Error::NotAStakedNode => write!(f, "Not a staked node"),
            Error::UnknownAddress => write!(f, "Unknown address"),
            Error::UnknownName => write!(f, "Unknown name"),
        }
    }
}

// The path followed by packets, and so the latency, depends on the source port (multipath routing).
// Using a fixed port by default makes successive measures comparable.
// The value is arbitrary, outside the default port range of validators and of the range of ephemeral ports.
const DEFAULT_SOURCE_PORT: u16 = 20488;

const LEADER_WINDOW: Duration = Duration::from_millis(4 * 200); // 4 slots
// Maximum rate of connections per second, all TPUs included. Above some rate, responses arrive faster
// than they can be processed, they are dropped by the system and measures are wrong.
const MAX_CONNECTION_RATE: f64 = 500.0;
const CONNECTION_TIMEOUT: Duration = Duration::from_millis(1600); // Quinn retransmission fires after 1s, so we need the timeout to be greater than 1s + highest RTT

/// Return the sorted latencies of successful connections.
///
/// Send `count` connection requests, spaced `interval` apart, which is at least 4 slots to give a good chance
/// that at least one request doesn't arrive when the validator is busy being leader.
/// Add a random temporization if requested.
async fn latency(endpoint: Endpoint, tpu_quic: SocketAddr, count: usize, interval: Duration, temporization: bool) -> Vec<u32> {
    let server_name = socket_addr_to_quic_server_name(tpu_quic);
    if temporization {
        let delay= rand::rng().random_range(Duration::ZERO..interval);
        sleep(delay).await;
    }
    let mut t = Instant::now();
    let mut lats = Vec::with_capacity(count);
    lats.extend(ping(&endpoint, &server_name, tpu_quic).await);
    for _ in 1..count {
        t = t.add(interval).max(Instant::now());
        sleep_until(t).await;
        lats.extend(ping(&endpoint, &server_name, tpu_quic).await);
    }
    lats.sort_unstable();
    lats
}

/// Return latency estimate and its variance from sorted latencies, or `None` if there is no latency.
/// The variance is `None` if there is only one latency.
///
/// We assume latencies follow a 2-parameter exponential distribution:
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

/// Return the weighted median of the first elements of `sorted`, which must be sorted.
/// The weight of an element is given by `weight` applied to its second element.
fn weighted_median(sorted: &[(u32, u32)], weight: impl Fn(u32) -> f64) -> Option<u32> {
    let total: f64 = sorted.iter().map(|&(_, i)| weight(i)).sum();
    let mut sum = 0.0;
    sorted.iter().find(|&&(_, i)| {
        sum += weight(i);
        sum >= total / 2.0
    }).map(|&(value, _)| value)
}

async fn ping(endpoint: &Endpoint, server_name: &String, tpu_quic: SocketAddr) -> Option<u32> {
    let connecting = endpoint.connect(tpu_quic, server_name).expect("Connection configuration error");
    if let Ok(Ok(connection)) = timeout(CONNECTION_TIMEOUT, connecting).await {
        // With a timeout of 1.6 s, rtt in µs should never overflow u32.
        let rtt: u32 = connection.rtt().as_micros().try_into().expect("rtt overflow");
        connection.close(VarInt::default(), &[]);
        Some(rtt/2)
    } else {
        None
    }
}

async fn decode_doublezero_info(dz_info: Response) -> Result<Vec<String>, &'static str> {
    let Ok(j) = dz_info.json::<Value>().await else { return Err("Invalid JSON") };
    if j["success"] != true { return Err("Failed") };
    let Some(validators) = j["data"]["validators"].as_array() else { return Err("No validators") };
    let accounts = validators.iter()
        .map(|validator| validator["account"].as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or("Validator without account")?;
    if accounts.is_empty() { return Err("No validators") };
    Ok(accounts)
}

const CONFIG_PROGRAM_ID: &str = "Config1111111111111111111111111111111111111";

/// Return the name and the identity of the validators which published their info on chain.
fn get_validator_names(rpc_client: &RpcClient) -> Vec<(String, String)> {
    let accounts: Vec<Value> = rpc_client
        .send(RpcRequest::GetProgramAccounts, json!([CONFIG_PROGRAM_ID, {"encoding": "jsonParsed"}]))
        .expect("Failed to get validators info");
    accounts.iter().filter_map(|account| {
        let parsed = &account["account"]["data"]["parsed"];
        if parsed["type"] != "validatorInfo" { return None };
        // Info is signed by the identity of the validator
        let identity = parsed["info"]["keys"].as_array()?.iter().find(|key| key["signer"] == true)?;
        let name = parsed["info"]["configData"]["name"].as_str()?.trim();
        Some((name.to_string(), identity["pubkey"].as_str()?.to_string()))
    }).collect()
}

/// Return the identity of the validator matching `name`, ignoring case, or all the matching
/// (name, identity) pairs if there are several ones.
/// If `name` ends with `*`, it matches names starting with what precedes.
fn find_validator<'a>(name: &str, validator_names: &'a [(String, String)]) -> Result<Option<&'a str>, Vec<&'a (String, String)>> {
    let name = name.trim().to_lowercase();
    let matches: Vec<_> = validator_names.iter().filter(|(validator_name, _)| {
        let validator_name = validator_name.to_lowercase();
        match name.strip_suffix('*') {
            Some(prefix) => validator_name.starts_with(prefix),
            None => validator_name == name,
        }
    }).collect();
    match matches.as_slice() {
        [] => Ok(None),
        [(_, pk)] => Ok(Some(pk)),
        _ => Err(matches),
    }
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
            let line = line.trim();
            if !line.is_empty() {
                destination.push(line.to_string());
            }
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

    let rpc_nodes = rpc_client.get_cluster_nodes().expect("Failed to get cluster nodes");
    let rpc_pk_nodes = HashMap::<&str, &RpcContactInfo>::from_iter(rpc_nodes.iter().map(|n| (n.pubkey.as_str(), n)));

    let mut errors = Errors(HashMap::new());

    let nodes_cnt = destination.len();
    let mut nodes_pk = Vec::new();
    let mut nodes_sa = Vec::new();
    let mut nodes_name = Vec::new();

    for str in destination.into_iter() {
        if let Ok(sock_addr) = str.parse::<SocketAddr>() {
            nodes_sa.push(sock_addr);
        } else if args.doublezero || str.parse::<Address>().is_ok() {
            nodes_pk.push(str);
        } else {
            nodes_name.push(str);
        }
    }

    if !nodes_name.is_empty() {
        // Names are not unique among all the validators whichever published one, so only consider
        // nodes of the cluster
        let mut validator_names = get_validator_names(&rpc_client);
        validator_names.retain(|(_, pk)| rpc_pk_nodes.contains_key(pk.as_str()));
        for name in nodes_name {
            match find_validator(&name, &validator_names) {
                Ok(Some(pk)) => nodes_pk.push(pk.to_string()),
                Ok(None) => errors.new(Error::UnknownName, 0),
                Err(matches) => {
                    let matches: Vec<_> = matches.iter().map(|(name, pk)| format!("{} {}", pk, name)).collect();
                    panic!("Several validators match name `{}`:\n{}", name, matches.join("\n"));
                }
            }
        }
    }

    let mut tpus: HashMap<SocketAddr, TPU> = HashMap::new();
    let mut total_stake = 0;

    let no_stake_weighting = nodes_cnt == 1 || args.no_stake_weighting;

    // Stake of staked nodes, only needed for stake-weighting
    let stakes = (!no_stake_weighting).then(|| {
        let mut stakes = HashMap::<String, u64>::new();
        for va in rpc_client.get_vote_accounts().expect("Failed to get vote accounts").current {
            if va.activated_stake != 0 {
                *stakes.entry(va.node_pubkey).or_default() += va.activated_stake;
            }
        }
        stakes
    });

    // Identities of the nodes to measure. Using a set, a node specified several times is only counted once.
    let mut ids = HashSet::<&str>::new();
    if nodes_cnt == 0 {
        match &stakes {
            Some(stakes) => ids.extend(stakes.keys().map(String::as_str)),
            None => ids.extend(rpc_nodes.iter().map(|n| n.pubkey.as_str())),
        }
    }
    ids.extend(nodes_pk.iter().map(String::as_str));
    for sock_addr in nodes_sa {
        // A TPU address stands for the nodes using it, limited to staked ones in case of stake-weighting
        let mut addr_ids = rpc_nodes.iter()
            .filter(|n| n.tpu_quic == Some(sock_addr))
            .map(|n| n.pubkey.as_str())
            .peekable();
        if addr_ids.peek().is_none() {
            if stakes.is_some() {
                errors.new(Error::UnknownAddress, 0);
            } else {
                // No known node uses this address, measure it anyway
                tpus.entry(sock_addr).or_default();
            }
            continue;
        }
        let mut addr_ids = addr_ids
            .filter(|&pk| stakes.as_ref().is_none_or(|stakes| stakes.contains_key(pk)))
            .peekable();
        if addr_ids.peek().is_some() {
            ids.extend(addr_ids);
        } else {
            errors.new(Error::NotAStakedNode, 0);
        }
    }

    for pk in ids {
        let stake = match &stakes {
            Some(stakes) => {
                let Some(&stake) = stakes.get(pk) else {
                    errors.new(Error::NotAStakedNode, 0);
                    continue;
                };
                stake
            }
            None => 0,
        };
        total_stake += stake;
        let Some(ci) = rpc_pk_nodes.get(pk) else {
            errors.new(Error::NoContactInfo, stake);
            continue;
        };
        let Some(sock_addr) = ci.tpu_quic else {
            errors.new(Error::NoTPU, stake);
            continue;
        };
        let tpu = tpus.entry(sock_addr).or_default();
        tpu.ids.push(pk.to_string());
        tpu.stake += stake;
    }

    let endpoint = new_quic_endpoint(&Keypair::new(), args.source_port).await;

    let temporization = tpus.len() > 1;
    // Connections to a TPU are spaced further apart when there are many TPUs, to limit the global rate of connections
    let interval = LEADER_WINDOW.max(Duration::from_secs_f64(tpus.len() as f64 / MAX_CONNECTION_RATE));
    for (sock_addr, tpu) in &mut tpus {
        tpu.join = Some(tokio::spawn(latency(endpoint.clone(), *sock_addr, args.count, interval, temporization)));
    }

    let mut lat_sum_w = 0.0;
    let mut lat_sum = 0.0;
    let mut lat_cnt = 0;
    let mut lat_stk = 0;

    // Latency estimates are independent, so the variance of their weighted mean is
    // sum(w_i^2 * var_i) / sum(w_i)^2
    let mut var_sum_w = 0.0;
    let mut var_sum = 0.0;

    // Jitter is the median of the absolute differences between two latencies of the same TPU, over
    // all the pairs of latencies of all TPUs. Being a difference, it doesn't depend on the latency
    // estimate, and being a median, it is robust against outliers.
    // Pairs are weighted so that the weight of a TPU doesn't depend on its number of pairs.
    let mut jit_diffs: Vec<(u32, u32)> = Vec::new(); // (difference, index of the TPU in `jit_weights`)
    let mut jit_weights: Vec<(f64, f64)> = Vec::new(); // weight of each pair of a TPU: (simple, stake-weighted)

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
                match join.await.map(|lats| (estimate_latency(&lats), lats)) {
                    Ok((None, _)) => {
                        errors.new_and_print_if(Error::ConnectionFailed, tpu.stake, args.details);
                    }
                    Ok((Some((lat, None)), _)) => {
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
                            errors.new_and_print_if(Error::OnlyOneSuccessfulConnection, tpu.stake, args.details);
                        }
                    }
                    Ok((Some((lat, Some(var))), lats)) => {
                        let pairs_cnt = (lats.len() * (lats.len() - 1) / 2) as f64;
                        let tpu_idx = jit_weights.len() as u32;
                        jit_weights.push((1.0 / pairs_cnt, tpu.stake as f64 / pairs_cnt));
                        for (i, lat_i) in lats.iter().enumerate() {
                            // Latencies are sorted
                            jit_diffs.extend(lats[i + 1..].iter().map(|lat_j| (lat_j - lat_i, tpu_idx)));
                        }
                        if total_stake > 0 {
                            lat_sum_w += lat * tpu.stake as f64;
                            lat_stk += tpu.stake;
                            var_sum_w += var * (tpu.stake as f64).powi(2);
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
                        errors.new_and_print_if(Error::ConnectionError, tpu.stake, args.details);
                    }
                }
            }
            None => {
            }
        }
    }

    jit_diffs.sort_unstable();

    if lat_cnt > 0 {
        if args.count > 1 {
            println!("Simple distance: {:.0} ± {:.0} µs", lat_sum / lat_cnt as f64, var_sum.sqrt() / lat_cnt as f64);
            if let Some(jitter) = weighted_median(&jit_diffs, |i| jit_weights[i as usize].0) {
                println!("Simple jitter: {} µs", jitter);
            }
        } else {
            println!("Simple distance: {:.0} µs", lat_sum / lat_cnt as f64);
        }
        if lat_stk > 0 {
            if args.count > 1 {
                println!("Stake-weighted distance: {:.0} ± {:.0} µs", lat_sum_w / lat_stk as f64, var_sum_w.sqrt() / lat_stk as f64);
                if let Some(jitter) = weighted_median(&jit_diffs, |i| jit_weights[i as usize].1) {
                    println!("Stake-weighted jitter: {} µs", jitter);
                }
            } else {
                println!("Stake-weighted distance: {:.0} µs", lat_sum_w / lat_stk as f64);
            }
            println!("Total stake: {} SOL", lat_stk / 1_000_000_000);
        }
        println!("Connection successful: {}", lat_cnt);
    }

    for (error, (cnt, stk)) in &errors.0 {
        if total_stake > 0 && ![Error::NotAStakedNode, Error::UnknownAddress, Error::UnknownName].contains(error) {
            println!("{}: {} ({:.2}% of total stake)", error, cnt, 100.0 * *stk as f64 / (total_stake as f64));
        } else {
            println!("{}: {}", error, cnt);
        }
    }
}