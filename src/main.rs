use core::{convert::Infallible, net::IpAddr, str::FromStr};
use std::process::Stdio;

use anyhow::Context as _;
use axum::{Router, routing::get};
use prometheus::{HistogramOpts, HistogramVec, IntCounterVec};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt as _, Interest},
    select,
    time::{Duration, Instant},
};

#[derive(Debug, Deserialize)]
struct Config {
    ping: Vec<PingConfig>,
    udpecho: Vec<UdpEchoConfig>,
}

#[derive(Debug, Deserialize)]
struct PingConfig {
    name: String,
    host: String,
    #[serde(default = "default_ping_interval")]
    interval: f64,
}

#[derive(Debug, Deserialize)]
struct UdpEchoConfig {
    name: String,
    host: String,
    #[serde(default = "default_udpecho_port")]
    port: u16,
    #[serde(default = "default_udpecho_interval")]
    interval: f64,
    payload: String,
}

fn default_ping_interval() -> f64 {
    1.
}

fn default_udpecho_interval() -> f64 {
    30.
}

fn default_udpecho_port() -> u16 {
    9999
}

#[derive(Debug)]
enum PingOutput {
    Header {
        host: String,
        ip_addr: IpAddr,
    },
    Reply {
        icmp_seq: u32,
        ttl: u8,
        time_ms: f64,
    },
    NoReply {
        icmp_seq: u32,
    },
    Other(String, anyhow::Error),
}

impl FromStr for PingOutput {
    type Err = Infallible;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let res: anyhow::Result<Self> = (|| {
            if let Some(header) = line.strip_prefix("PING ") {
                let mut parts = header.split_whitespace();
                let host = parts
                    .next()
                    .context("missing host in ping header")?
                    .to_string();
                let ip_addr = parts.next().context("missing ip in ping header")?;
                let ip_addr = ip_addr
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .parse()
                    .context("invalid ip address in ping header")?;

                Ok(PingOutput::Header { host, ip_addr })
            } else if let Some(reply) = line.strip_prefix("64 bytes from ") {
                let mut parts = reply.split_whitespace();
                let _host = parts.next().context("missing host in ping reply")?;
                let icmp_seq = parts.next().context("missing icmp_seq in ping reply")?;
                let ttl = parts.next().context("missing ttl in ping reply")?;
                let time = parts.next().context("missing time in ping reply")?;

                let icmp_seq = icmp_seq
                    .strip_prefix("icmp_seq=")
                    .context("invalid icmp_seq in ping reply")?
                    .parse()
                    .context("invalid icmp_seq number in ping reply")?;
                let ttl = ttl
                    .strip_prefix("ttl=")
                    .context("invalid ttl in ping reply")?
                    .parse()
                    .context("invalid ttl number in ping reply")?;
                let time_ms = time
                    .strip_prefix("time=")
                    .context("invalid time in ping reply")?
                    .parse()
                    .context("invalid time number in ping reply")?;

                Ok(PingOutput::Reply {
                    icmp_seq,
                    ttl,
                    time_ms,
                })
            } else if let Some(icmp_seq) = line.strip_prefix("no answer yet for icmp_seq=") {
                let icmp_seq = icmp_seq
                    .parse()
                    .context("invalid icmp_seq number in no-reply ping")?;

                Ok(PingOutput::NoReply { icmp_seq })
            } else {
                Err(anyhow::anyhow!("unrecognized ping output"))
            }
        })();

        match res {
            Ok(output) => Ok(output),
            Err(e) => Ok(PingOutput::Other(line.to_string(), e)),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config_path = "/etc/network-quality-exporter.toml";

    if !tokio::fs::try_exists(config_path).await? {
        panic!("Config file not found at {}", config_path);
    }

    tracing::info!("Loading config from {}", config_path);
    let config_str = tokio::fs::read_to_string(config_path).await?;
    let config: Config = toml::from_str(&config_str)?;

    tracing::debug!("Config: {:#?}", config);

    let time_histograms = HistogramVec::new(
        HistogramOpts::new("time_seconds", "Ping time in seconds")
            .namespace("netquality")
            .subsystem("ping"),
        &["host", "name"],
    )?;

    let missed_pings = IntCounterVec::new(
        prometheus::Opts::new("missed_pings_count", "Total number of missed pings")
            .namespace("netquality")
            .subsystem("ping"),
        &["host", "name"],
    )?;

    prometheus::default_registry().register(Box::new(time_histograms.clone()))?;
    prometheus::default_registry().register(Box::new(missed_pings.clone()))?;

    let config_count = (config.ping.len() + config.udpecho.len()) as u32;

    for ping in config.ping {
        let time_histogram =
            time_histograms.get_metric_with_label_values(&[&ping.host, &ping.name])?;

        let missed_pings_counter =
            missed_pings.get_metric_with_label_values(&[&ping.host, &ping.name])?;

        tokio::spawn(async move {
            tracing::info!("Starting ping to {} every {}s", ping.host, ping.interval);

            let proc = tokio::process::Command::new("ping")
                .arg("-Oni")
                .arg(ping.interval.to_string())
                .arg(&ping.host)
                .stdout(Stdio::piped())
                .spawn()
                .expect("failed to start ping command");

            let stdout = proc.stdout.unwrap();

            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Some(line) = reader.next_line().await.unwrap() {
                let output: PingOutput = line.parse().unwrap();
                tracing::debug!("Ping output: {:?}", output);
                match output {
                    PingOutput::Header { .. } => {}
                    PingOutput::Reply { time_ms, .. } => {
                        let time_s = time_ms / 1000.;
                        time_histogram.observe(time_s);
                    }
                    PingOutput::NoReply { .. } => {
                        missed_pings_counter.inc();
                    }
                    PingOutput::Other(line, error) => {
                        tracing::warn!("Unrecognized ping output: {} ({})", line, error);
                    }
                }
            }
        });

        // naive attempt at staggering
        tokio::time::sleep(Duration::from_secs(1) / config_count).await;
    }

    let udpecho_time_histograms = HistogramVec::new(
        HistogramOpts::new("time_seconds", "UDP echo time in seconds")
            .namespace("netquality")
            .subsystem("udpecho"),
        &["host", "name"],
    )?;

    let udpecho_missed_counters = IntCounterVec::new(
        prometheus::Opts::new(
            "missed_udpecho_count",
            "Total number of missed UDP echo replies",
        )
        .namespace("netquality")
        .subsystem("udpecho"),
        &["host", "name"],
    )?;

    prometheus::default_registry().register(Box::new(udpecho_time_histograms.clone()))?;
    prometheus::default_registry().register(Box::new(udpecho_missed_counters.clone()))?;

    for udpecho in config.udpecho {
        let udpecho_time_histogram = udpecho_time_histograms
            .get_metric_with_label_values(&[&udpecho.host, &udpecho.name])?;

        let udpecho_missed_counter = udpecho_missed_counters
            .get_metric_with_label_values(&[&udpecho.host, &udpecho.name])?;

        tokio::spawn(async move {
            tracing::info!(
                "Starting UDP echo to {}:{} every {}s",
                udpecho.host,
                udpecho.port,
                udpecho.interval
            );

            let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
                .await
                .expect("failed to bind udp socket");

            sock.connect((udpecho.host.as_str(), udpecho.port))
                .await
                .expect("failed to connect udp socket");

            let payload = udpecho.payload.into_bytes();
            let mut buf = vec![0u8; 2048]; // udp packets shouldn't be larger than 1500 bytes or so

            let mut ticker = tokio::time::interval(Duration::from_secs_f64(udpecho.interval));
            ticker.tick().await; // skip initial immediate tick

            loop {
                while sock.try_recv(&mut buf).is_ok() {
                    // drain any incoming packets
                }

                sock.ready(Interest::WRITABLE)
                    .await
                    .expect("failed to wait for udp socket writable");

                let start = Instant::now();
                if let Err(e) = sock.send(&payload).await {
                    tracing::warn!("failed to send udp packet to {}: {}", udpecho.host, e);
                    continue;
                }

                select! {
                    _ = ticker.tick() => {
                        tracing::warn!("UDP echo to {} timed out", udpecho.host);

                        // timeout
                        udpecho_missed_counter.inc();
                    }
                    result = sock.recv(&mut buf) => {
                        match result {
                            Ok(_) => {
                                let elapsed = start.elapsed();
                                let elapsed_sec = elapsed.as_secs_f64();

                                tracing::debug!("Received UDP echo reply from {} in {}s", udpecho.host, elapsed_sec);

                                udpecho_time_histogram.observe(elapsed_sec);

                                ticker.tick().await;
                            }
                            Err(e) => {
                                tracing::error!("failed to receive udp packet from {}: {}", udpecho.host, e);
                            }
                        }
                    }
                }
            }
        });

        // naive attempt at staggering
        tokio::time::sleep(Duration::from_secs(1) / config_count).await;
    }

    let app = Router::new().route(
        "/metrics",
        get(|| async {
            tracing::debug!("Serving /metrics");
            let encoder = prometheus::TextEncoder::new();
            let metric_families = prometheus::gather();
            encoder.encode_to_string(&metric_families).unwrap()
        }),
    );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:9635").await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
