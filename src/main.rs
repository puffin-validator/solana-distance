mod quic;

use crate::quic::{new_quic_endpoint, socket_addr_to_quic_server_name};
use crate::Error::{ConnectionError, ConnectionFailed, NoContactInfo, NoTPU, NotAStakedNode};
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
use reqwest::blocking::Response;
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
    #[arg(short, long, default_value_t = 5, help = "Number of connection attempts, one attempt is performed every 1,8 secs")]
    count: usize,
    #[arg(short, long, help = "URL of the RPC where cluster info is fetched from", default_value="https://api.mainnet-beta.solana.com")]
    rpc: String,
    #[arg(short='2', long, help = "Measure the distance to the a Doublezero network passed as an optional argument [default: mainnet]")]
    doublezero: bool,
}

struct TPU {
    stake: u64,
    join: Option<JoinHandle<u128>>,
    ids: Vec<String>,
}

#[derive(Eq, Hash, PartialEq)]
enum Error {
    ConnectionFailed,
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
}
impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            ConnectionError => write!(f, "Connection error"),
            ConnectionFailed => write!(f, "Connection failed"),
            NoContactInfo => write!(f, "No contact info"),
            NoTPU => write!(f, "No TPU"),
            NotAStakedNode => write!(f, "Not a staked node"),
        }
    }
}

const LEADER_WINDOW: Duration = Duration::from_millis(4 * 400); // 4 slots
const CONNECTION_TIMEOUT: Duration = LEADER_WINDOW;

/// Send `count` connection requests, spaced 4 slots apart, to give a good chance that at least one request
/// doesn't arrive when the validator is busy being leader.
/// Take the minimum delay.
/// Add a random temporization if requested.
async fn rtt(endpoint: Endpoint, tpu_quic: SocketAddr, count: usize, temporization: bool) -> u128 {
    let server_name = socket_addr_to_quic_server_name(tpu_quic);
    if temporization {
        let delay= rand::rng().random_range(Duration::ZERO..LEADER_WINDOW);
        sleep(delay).await;
    }
    let mut t = tokio::time::Instant::now();
    let mut rtt_min = ping(&endpoint, &server_name, tpu_quic).await;
    for _ in 1..count {
        t = t.add(LEADER_WINDOW);
        sleep_until(t).await;
        rtt_min = rtt_min.min(ping(&endpoint, &server_name, tpu_quic).await);
    }
    rtt_min
}

async fn ping(endpoint: &Endpoint, server_name: &String, tpu_quic: SocketAddr) -> u128 {
    let connecting = endpoint.connect(tpu_quic, server_name).expect("Connection configuration error");
    if let Ok(Ok(connection)) = timeout(CONNECTION_TIMEOUT, connecting).await {
        let rtt = connection.rtt().as_micros();
        connection.close(VarInt::default(), &[]);
        rtt
    } else {
        u128::MAX
    }
}

fn decode_doublezero_info(dz_info: Response) -> Result<Vec<String>, &'static str> {
    let Ok(j) = dz_info.json::<Value>() else { return Err("Invalid JSON") };
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
        let dz_info = reqwest::blocking::get(&url).expect("Cannot send request to Doublezero API");
        destination = decode_doublezero_info(dz_info).unwrap_or_else(|e| panic!("Failed to decode Doublezero API response: {}", e));
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


    let endpoint = new_quic_endpoint(&Keypair::new(), 0).await;

    let temporization = tpus.len() > 1;
    for (sock_addr, tpu) in &mut tpus {
        tpu.join = Some(tokio::spawn(rtt(endpoint.clone(), *sock_addr, args.count, temporization)));
    }

    let mut distance_sum_w = 0;
    let mut distance_sum = 0;
    let mut distance_cnt = 0;
    let mut distance_stk = 0;

    for (sock_addr, tpu) in tpus {
        match tpu.join {
            Some(join) => {
                if args.details {
                    if total_stake > 0 {
                        print!("{:21} {:>9} SOL {:?}", sock_addr, tpu.stake / 1_000_000_000, tpu.ids);
                    } else {
                        print!("{:21} {:?}", sock_addr, tpu.ids);
                    }
                }
                match join.await {
                    Ok(u128::MAX) => {
                        errors.new(ConnectionFailed, tpu.stake);
                        if args.details {
                            println!(" Failed");
                        }
                    }
                    Ok(rtt) => {
                        let distance = rtt/2;
                        if total_stake > 0 {
                            distance_sum_w += distance * tpu.stake as u128;
                            distance_stk += tpu.stake;
                        }
                        distance_sum += distance;
                        distance_cnt += 1;
                        if args.details {
                            println!(" {} µs", distance);
                        }
                    }
                    Err(_) => {
                        errors.new(ConnectionError, tpu.stake);
                    }
                }
            }
            None => {
            }
        }
    }

    if distance_cnt > 0 {
        println!("Simple distance: {} µs", distance_sum / distance_cnt as u128);
        println!("Connection successful: {}", distance_cnt);
        if total_stake > 0 {
            println!("Stake-weighted distance: {} µs", distance_sum_w / distance_stk as u128);
            println!("Total stake: {} SOL", distance_stk / 1_000_000_000);
        }
    }

    for (error, (cnt, stk)) in &errors.0 {
        if total_stake > 0 && *error != NotAStakedNode {
            println!("{}: {} ({:.2}% of total stake)", error, cnt, 100.0 * *stk as f64 / (total_stake as f64));
        } else {
            println!("{}: {}", error, cnt);
        }
    }
}