use std::{
    collections::HashMap,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    str::FromStr as _,
    sync::Arc,
};

use base64::prelude::*;
use clap::Parser;
use divvun_runtime::{Bundle, modules::Input};
use futures_util::StreamExt;
use geoipd::GeoIpLookup;
use poem::{
    EndpointExt, IntoResponse, Request, Response, Route, Server, get, handler,
    http::StatusCode,
    listener::{TcpListener, UnixListener},
    middleware::Cors,
    post,
    web::{Data, Html, Json, Query},
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to speech bundle or directory containing tts.drb
    #[arg(required = true)]
    bundle_path: PathBuf,

    /// Address to bind to. Formats: tcp://host:port or unix:///path/to/socket
    #[arg(short, long, env = "ADDRESS", default_value = "tcp://127.0.0.1:4000")]
    address: String,

    /// Configuration file (ignored if bundle_path is a directory)
    #[arg(short, long, env = "CONFIG", default_value = "config.toml")]
    config_path: PathBuf,

    #[arg(long, env = "MAXMIND_ACCOUNT_ID")]
    maxmind_account_id: Option<String>,

    #[arg(long, env = "MAXMIND_LICENSE_KEY")]
    maxmind_license_key: Option<String>,
}

type Config = HashMap<String, VoiceConfig>;
#[derive(Debug, serde::Deserialize)]
struct VoiceConfig {
    language: usize,
    #[allow(dead_code)]
    speakers: Vec<usize>,
}

#[derive(Debug)]
enum ListenerAddress {
    Tcp { host: String, port: u16 },
    Unix { path: String },
}

impl Display for ListenerAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerAddress::Tcp { host, port } => write!(f, "http://{}:{}", host, port),
            ListenerAddress::Unix { path } => write!(f, "path: {}", path),
        }
    }
}

impl ListenerAddress {
    fn parse(address: &str) -> anyhow::Result<Self> {
        if let Some(tcp_part) = address.strip_prefix("tcp://") {
            let (host, port_str) = tcp_part.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("Invalid TCP address format. Expected tcp://host:port")
            })?;

            let port: u16 = port_str
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid port number: {}", port_str))?;

            Ok(ListenerAddress::Tcp {
                host: host.to_string(),
                port,
            })
        } else if let Some(unix_part) = address.strip_prefix("unix://") {
            Ok(ListenerAddress::Unix {
                path: unix_part.to_string(),
            })
        } else {
            Err(anyhow::anyhow!(
                "Invalid address format. Use tcp://host:port or unix:///path/to/socket"
            ))
        }
    }
}

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
    #[serde(default)]
    country: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProcessQuery {
    #[serde(default)]
    speaker: usize,
    #[serde(default)]
    language: usize,
    #[serde(default)]
    text: bool,
}

pub async fn country_lookup(geoip: &Arc<GeoIpLookup>, request: &Request) -> Option<String> {
    let remote_addr = request
        .header("X-Real-IP")
        .and_then(|x| {
            Ipv4Addr::from_str(x)
                .map(IpAddr::V4)
                .or_else(|_| Ipv6Addr::from_str(x).map(IpAddr::V6))
                .ok()
        })
        .or_else(|| {
            request.header("X-Forwarded-For").and_then(|x| {
                Ipv4Addr::from_str(x)
                    .map(IpAddr::V4)
                    .or_else(|_| Ipv6Addr::from_str(x).map(IpAddr::V6))
                    .ok()
            })
        })
        .or_else(|| request.remote_addr().as_socket_addr().map(|x| x.ip()));

    let Some(ip_addr) = remote_addr else {
        return None;
    };

    let Some(country) = geoip.lookup_country(ip_addr).await.ok() else {
        return None;
    };

    country
}

fn parse_accept_languages(header: &str) -> Vec<(String, f32)> {
    let mut languages = Vec::new();

    for part in header.split(',') {
        let mut segments = part.trim().split(';');
        if let Some(lang) = segments.next() {
            let q_value = segments
                .find_map(|s| {
                    if s.trim().starts_with("q=") {
                        s.trim()[2..].parse::<f32>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(1.0);
            languages.push((lang.to_string(), q_value));
        }
    }

    // Sort by quality value (q-value) in descending order
    languages.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    languages
}

async fn derive_country(request: &Request) -> Option<String> {
    if let Some(header) = request.header("Accept-Language") {
        let languages = parse_accept_languages(header);
        for (lang, _) in languages {
            let chunks = lang.split('-').collect::<Vec<_>>();
            if chunks.len() == 1 {
                match chunks[0] {
                    "nb" | "nn" => return Some("NO".to_string()),
                    "sv" => return Some("SE".to_string()),
                    "da" => return Some("DK".to_string()),
                    "is" => return Some("IS".to_string()),
                    "fi" => return Some("FI".to_string()),
                    _ => continue,
                }
            } else if chunks.len() == 2 {
                return Some(chunks[1].to_uppercase());
            } else if chunks.len() == 3 {
                return Some(chunks[2].to_uppercase());
            }
        }
    }

    if let Some(geoip) = request.data::<Arc<GeoIpLookup>>() {
        if let Some(country) = country_lookup(geoip, request).await {
            return Some(country);
        }
    }

    None
}

enum StreamData {
    Text(Vec<String>),
    Audio(Vec<i16>),
}

#[handler]
async fn process(
    Data(holder): Data<&Arc<PipelineHolder>>,
    Query(query): Query<ProcessQuery>,
    Json(body): Json<ProcessInput>,
    req: &Request,
) -> impl IntoResponse {
    let time_start = std::time::Instant::now();
    let country = match body.country.as_deref() {
        Some("") => None,
        Some(v) => Some(v.to_string()),
        None => derive_country(req).await,
    };

    let text = match holder.text.get(&query.language) {
        Some(text) => text,
        None => {
            return Json(serde_json::json!({
                "error": "Language not found".to_string()
            }))
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
        }
    };

    let mut speech_pipeline = match holder
        .speech
        .create(serde_json::json!({
            "language": query.language,
            "speaker": query.speaker,
        }))
        .await
    {
        Ok(pipeline) => pipeline,
        Err(e) => {
            return Json(serde_json::json!({
                "error": e.to_string()
            }))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
    };

    let mut config = serde_json::json!({});

    if let Some(country) = country {
        config.as_object_mut().unwrap().insert(
            "streamcmd-value".to_string(),
            serde_json::json!({
                "country": country,
            }),
        );
    }

    let mut text_pipeline = match text.create(config).await {
        Ok(pipeline) => pipeline,
        Err(e) => {
            return Json(serde_json::json!({
                "error": e.to_string()
            }))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
    };

    let mut stream = text_pipeline.forward(Input::String(body.text)).await;

    #[allow(for_loops_over_fallibles)]
    let mut stream = Box::pin(async_stream::stream! {
        while let Some(output) = stream.next().await {
            let output = match output {
                Ok(output) => output,
                Err(e) => {
                    yield Err(e);
                    return;
                }
            };

            match output {
                Input::String(ref s) => {
                    yield Ok(StreamData::Text(vec![s.clone()]));
                }
                Input::ArrayString(ref s) => {
                    yield Ok(StreamData::Text(s.clone()));
                }
                _ => {}
            }

            let mut inner_stream = speech_pipeline.forward(output).await;

            for output in inner_stream.next().await {
                match output {
                    Ok(output) => {
                        let output = output.try_into_bytes().unwrap();
                        let mut reader = hound::WavReader::new(std::io::Cursor::new(output)).unwrap();
                        let samples = reader.samples::<i16>().collect::<Result<Vec<_>, _>>();
                        let samples = match samples {
                            Ok(samples) => samples,
                            Err(e) => {
                                yield Err(divvun_runtime::modules::Error(e.to_string()));
                                return;
                            }
                        };
                        yield Ok(StreamData::Audio(samples));
                    }
                    Err(e) => {
                        yield Err(e);
                    }
                }
            }
        }
    });

    let mut bytes = Vec::new();
    let mut texts = Vec::new();
    while let Some(output) = stream.next().await {
        match output {
            Ok(StreamData::Text(text)) => {
                if query.text {
                    texts.extend(text);
                }
            }
            Ok(StreamData::Audio(output)) => bytes.extend(output),
            Err(e) => {
                return Json(serde_json::json!({
                    "error": e.to_string()
                }))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
            }
        }
    }

    if bytes.is_empty() {
        return Json(serde_json::json!({
            "error": "No output"
        }))
        .into_response();
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 22050,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let out = Vec::with_capacity(bytes.len() / 2 + 1);
    let mut out = std::io::Cursor::new(out);

    let mut writer = hound::WavWriter::new(&mut out, spec).unwrap();
    for data in bytes {
        writer.write_sample(data).unwrap();
    }

    drop(writer);

    let time = time_start.elapsed().as_millis();

    let out = out.into_inner();
    tracing::info!(
        time_ms = time,
        language = query.language,
        speaker = query.speaker,
        "generated {} bytes.",
        out.len()
    );

    let mut response = Response::builder()
        .header("Content-Type", "audio/wav")
        .header("X-Divvun-Language", query.language.to_string())
        .header("X-Divvun-Voice", query.speaker.to_string());

    // Add processed text as base64-encoded header if requested
    if query.text && !texts.is_empty() {
        let buffer = texts
            .join("\n")
            .trim()
            .encode_utf16()
            .map(|u| u.to_le_bytes())
            .flatten()
            .collect::<Vec<u8>>();
        let encoded_text = base64::prelude::BASE64_STANDARD.encode(buffer);
        response = response.header("X-Divvun-Text", encoded_text);
    }

    response.body(out).into_response()
}

const PAGE: &str = include_str!("../index.html");

#[handler]
async fn process_get() -> impl IntoResponse {
    Html(PAGE).into_response()
}

#[handler]
async fn health_get() -> impl IntoResponse {
    "OK".to_string().into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Ok(run().await?)
}

struct PipelineHolder {
    speech: Bundle,
    text: HashMap<usize, Bundle>,
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let (config_path, speech_bundle_path, bundle_base_path) = if args.bundle_path.is_dir() {
        // Directory mode: use tts.drb and config.toml from directory
        let config_path = args.bundle_path.join("config.toml");
        let speech_bundle_path = args.bundle_path.join("tts.drb");
        (config_path, speech_bundle_path, args.bundle_path.clone())
    } else {
        // File mode: use provided bundle file and config
        let bundle_base_path = args.bundle_path.parent().unwrap().to_path_buf();
        (
            args.config_path.clone(),
            args.bundle_path.clone(),
            bundle_base_path,
        )
    };

    tracing::info!("Loading config from {}", config_path.display());
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    tracing::info!(
        "Loading speech bundle from {}",
        speech_bundle_path.display()
    );
    let speech = Bundle::from_bundle(&speech_bundle_path)?;

    let mut text = HashMap::new();

    for (language, voice_config) in config {
        tracing::info!("Loading text bundle for language {}", language);
        let bundle = Bundle::from_bundle(bundle_base_path.join(format!("text-{}.drb", language)))?;
        text.insert(voice_config.language, bundle);
    }

    let holder = PipelineHolder { speech, text };

    let geoip = if let (Some(account_id), Some(license_key)) =
        (args.maxmind_account_id, args.maxmind_license_key)
    {
        tracing::info!("Loading GeoIP database");
        Some(GeoIpLookup::new(account_id, license_key).await?)
    } else {
        None
    };

    let app = Route::new()
        .at("/", post(process).get(process_get))
        .at("/health", get(health_get))
        .data(Arc::new(holder))
        .data_opt(geoip)
        .with(Cors::default());

    let address = ListenerAddress::parse(&args.address)?;

    tracing::info!("Starting server on {}", address);
    match address {
        ListenerAddress::Unix { path } => {
            Server::new(UnixListener::bind(path)).run(app).await?;
        }
        ListenerAddress::Tcp { host, port } => {
            Server::new(TcpListener::bind((host, port)))
                .run(app)
                .await?;
        }
    }

    Ok(())
}
