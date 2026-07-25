#!/usr/bin/env rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0.103"
//! bincode = { version = "2", features = ["serde"] }
//! detcore = { path = "../../detcore" }
//! futures = "0.3.31"
//! reverie = { version = "0.1.0", git = "https://github.com/rrnewton/reverie.git", rev = "0e77d260a84083f462ba10b986c8a72ab8f92758" }
//! serde = { version = "1.0.219", features = ["derive"] }
//! serde_json = "1.0.140"
//! tarpc = { version = "0.37.0", features = ["serde-transport-bincode", "tokio1", "unix"] }
//! tokio = { version = "1.52.4", features = ["net", "rt", "sync"] }
//! ```

use std::any::type_name;
use std::hint::black_box;
use std::io::Read;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;
use std::time::Instant;

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#708): Review the transport benchmark methodology.
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use futures::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tarpc::client;
use tarpc::context;
use tarpc::server::BaseChannel;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;

type ActualRequest = <detcore::GlobalState as reverie::GlobalTool>::Request;
type ActualResponse = <detcore::GlobalState as reverie::GlobalTool>::Response;

#[tarpc::service]
trait PingPong {
    async fn ping(request: ActualRequest) -> ActualResponse;
}

#[derive(Clone)]
struct PingPongServer {
    response: ActualResponse,
}

impl PingPong for PingPongServer {
    async fn ping(self, _: context::Context, request: ActualRequest) -> ActualResponse {
        black_box(request);
        black_box(self.response.clone())
    }
}

#[derive(Clone, Copy)]
struct Options {
    iterations: usize,
    warmup: usize,
}

struct Measurement {
    elapsed: Duration,
    latencies_ns: Vec<u64>,
}

struct Summary {
    p50_ns: u64,
    p99_ns: u64,
    mean_ns: f64,
    rps: f64,
}

fn main() -> Result<()> {
    let options = parse_options()?;
    let request = sample_request()?;
    let response = sample_response()?;

    validate_payloads(&request, &response)?;

    println!("RPC payload types:");
    println!("  request:  {}", type_name::<ActualRequest>());
    println!("  response: {}", type_name::<ActualResponse>());
    println!(
        "  bincode2 payload bytes: request={}, response={}",
        encode(&request)?.len(),
        encode(&response)?.len()
    );
    println!(
        "Benchmark: {} measured round trips after {} warmup round trips\n",
        options.iterations, options.warmup
    );

    let raw = measure_raw(options, request.clone(), response.clone())?;
    let tarpc = measure_tarpc(options, request, response)?;
    let raw_summary = summarize(&raw);
    let tarpc_summary = summarize(&tarpc);

    println!("| Transport | p50 (us) | p99 (us) | Mean (us) | Throughput (RPS) |");
    println!("| --- | ---: | ---: | ---: | ---: |");
    print_summary("raw UDS + bincode 2", &raw_summary);
    print_summary("tarpc + bincode", &tarpc_summary);
    println!();
    println!(
        "tarpc/raw: {:.2}x p50 latency, {:.2}x p99 latency, {:.2}x throughput",
        ratio(tarpc_summary.p50_ns as f64, raw_summary.p50_ns as f64),
        ratio(tarpc_summary.p99_ns as f64, raw_summary.p99_ns as f64),
        ratio(tarpc_summary.rps, raw_summary.rps)
    );
    println!(
        "incremental tarpc median overhead: {:.3} us per RPC",
        (tarpc_summary.p50_ns as f64 - raw_summary.p50_ns as f64) / 1_000.0
    );

    Ok(())
}

fn parse_options() -> Result<Options> {
    let mut options = Options {
        iterations: 100_000,
        warmup: 10_000,
    };
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--iterations" => {
                options.iterations = parse_count("--iterations", args.next())?;
            }
            "--warmup" => {
                options.warmup = parse_count("--warmup", args.next())?;
            }
            "-h" | "--help" => {
                println!(
                    "Usage: ping_pong.rs [--iterations N] [--warmup N]\n\n\
                     Measures sequential request/response RPCs over raw UDS+bincode and \
                     tarpc+bincode. Defaults: --iterations 100000 --warmup 10000."
                );
                std::process::exit(0);
            }
            _ => bail!("unknown argument {arg:?}; use --help"),
        }
    }

    if options.iterations == 0 {
        bail!("--iterations must be greater than zero");
    }
    Ok(options)
}

fn parse_count(flag: &str, value: Option<String>) -> Result<usize> {
    value
        .with_context(|| format!("{flag} requires a value"))?
        .parse()
        .with_context(|| format!("{flag} requires a non-negative integer"))
}

fn sample_request() -> Result<ActualRequest> {
    serde_json::from_value(json!([
        {
            "syscalls": 1,
            "syscall_nanos": 1_000,
            "rcbs": 64,
            "nondet_instrs": 0,
            "extra_nanos": 0,
            "starting_micros": 0,
            "multiplier": 1.0
        },
        "GlobalTimeLowerBound"
    ]))
    .context("constructing the actual GlobalRequest fixture")
}

fn sample_response() -> Result<ActualResponse> {
    serde_json::from_value(json!([null, { "GlobalTimeLowerBound": 1_000_000 }]))
        .context("constructing the actual GlobalResponse fixture")
}

fn validate_payloads(request: &ActualRequest, response: &ActualResponse) -> Result<()> {
    let request_bytes = encode(request)?;
    let response_bytes = encode(response)?;
    let _: ActualRequest = decode(&request_bytes)?;
    let _: ActualResponse = decode(&response_bytes)?;
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, bincode::config::legacy())
        .context("serializing a bincode frame")
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, consumed): (T, usize) =
        bincode::serde::decode_from_slice(bytes, bincode::config::legacy())
            .context("deserializing a bincode frame")?;
    if consumed != bytes.len() {
        bail!(
            "bincode decoder consumed {consumed} of {} frame bytes",
            bytes.len()
        );
    }
    Ok(value)
}

fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let payload = encode(value)?;
    let len: u32 = payload
        .len()
        .try_into()
        .context("RPC frame exceeds u32 length prefix")?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&payload)?;
    Ok(())
}

fn read_frame<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let mut payload = vec![0_u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut payload)?;
    decode(&payload)
}

fn measure_raw(
    options: Options,
    request: ActualRequest,
    response: ActualResponse,
) -> Result<Measurement> {
    let (mut client_stream, mut server_stream) = UnixStream::pair()?;
    let total = options
        .warmup
        .checked_add(options.iterations)
        .context("iteration count overflow")?;
    let server = thread::spawn(move || -> Result<()> {
        for _ in 0..total {
            let incoming: ActualRequest = read_frame(&mut server_stream)?;
            black_box(incoming);
            let outgoing = black_box(response.clone());
            write_frame(&mut server_stream, &outgoing)?;
        }
        Ok(())
    });

    for _ in 0..options.warmup {
        write_frame(&mut client_stream, &request)?;
        let incoming: ActualResponse = read_frame(&mut client_stream)?;
        black_box(incoming);
    }

    let mut latencies_ns = Vec::with_capacity(options.iterations);
    let measurement_start = Instant::now();
    for _ in 0..options.iterations {
        let started = Instant::now();
        write_frame(&mut client_stream, &request)?;
        let incoming: ActualResponse = read_frame(&mut client_stream)?;
        black_box(incoming);
        latencies_ns.push(duration_ns(started.elapsed()));
    }
    let elapsed = measurement_start.elapsed();

    server
        .join()
        .map_err(|_| anyhow::anyhow!("raw UDS server thread panicked"))??;
    Ok(Measurement {
        elapsed,
        latencies_ns,
    })
}

fn measure_tarpc(
    options: Options,
    request: ActualRequest,
    response: ActualResponse,
) -> Result<Measurement> {
    let (client_stream, server_stream) = UnixStream::pair()?;
    client_stream.set_nonblocking(true)?;
    server_stream.set_nonblocking(true)?;

    let server = thread::spawn(move || -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let stream = tokio::net::UnixStream::from_std(server_stream)?;
            let transport = tarpc::serde_transport::Transport::from((stream, Bincode::default()));
            BaseChannel::with_defaults(transport)
                .execute(PingPongServer { response }.serve())
                .for_each(|request| async move {
                    tokio::spawn(request);
                })
                .await;
            Ok::<_, anyhow::Error>(())
        })
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let measurement = runtime.block_on(async move {
        let stream = tokio::net::UnixStream::from_std(client_stream)?;
        let transport = tarpc::serde_transport::Transport::from((stream, Bincode::default()));
        let client = PingPongClient::new(client::Config::default(), transport).spawn();

        for _ in 0..options.warmup {
            let incoming = client
                .ping(context::current(), request.clone())
                .await
                .context("tarpc warmup RPC failed")?;
            black_box(incoming);
        }

        let mut latencies_ns = Vec::with_capacity(options.iterations);
        let measurement_start = Instant::now();
        for _ in 0..options.iterations {
            let rpc_request = request.clone();
            let rpc_context = context::current();
            let started = Instant::now();
            let incoming = client
                .ping(rpc_context, rpc_request)
                .await
                .context("tarpc measured RPC failed")?;
            black_box(incoming);
            latencies_ns.push(duration_ns(started.elapsed()));
        }
        let elapsed = measurement_start.elapsed();
        drop(client);
        Ok::<_, anyhow::Error>(Measurement {
            elapsed,
            latencies_ns,
        })
    })?;
    drop(runtime);

    server
        .join()
        .map_err(|_| anyhow::anyhow!("tarpc server thread panicked"))??;
    Ok(measurement)
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().try_into().unwrap_or(u64::MAX)
}

fn summarize(measurement: &Measurement) -> Summary {
    let mut latencies = measurement.latencies_ns.clone();
    latencies.sort_unstable();
    let samples = latencies.len();
    Summary {
        p50_ns: percentile(&latencies, 50),
        p99_ns: percentile(&latencies, 99),
        mean_ns: latencies.iter().map(|&value| value as f64).sum::<f64>() / samples as f64,
        rps: samples as f64 / measurement.elapsed.as_secs_f64(),
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1)]
}

fn print_summary(name: &str, summary: &Summary) {
    println!(
        "| {name} | {:.3} | {:.3} | {:.3} | {:.0} |",
        summary.p50_ns as f64 / 1_000.0,
        summary.p99_ns as f64 / 1_000.0,
        summary.mean_ns / 1_000.0,
        summary.rps
    );
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        f64::NAN
    } else {
        numerator / denominator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_the_real_detcore_rpc_types_and_round_trip() {
        assert!(type_name::<ActualRequest>().contains("detcore::tool_global::GlobalRequest"));
        assert!(type_name::<ActualResponse>().contains("detcore::tool_global::GlobalResponse"));

        let request = sample_request().unwrap();
        let response = sample_response().unwrap();
        validate_payloads(&request, &response).unwrap();
    }

    #[test]
    fn raw_frame_uses_shared_big_endian_length_prefix() {
        let request = sample_request().unwrap();
        let expected_len: u32 = encode(&request).unwrap().len().try_into().unwrap();
        let (mut writer, mut reader) = UnixStream::pair().unwrap();

        write_frame(&mut writer, &request).unwrap();

        let mut header = [0_u8; 4];
        reader.read_exact(&mut header).unwrap();
        assert_eq!(header, expected_len.to_be_bytes());
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 99), 99);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 99), 7);
    }
}
