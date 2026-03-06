use std::{
    collections::HashMap,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::Arc,
};

use base64::prelude::*;
use clap::{Parser, Subcommand};
use divvun_runtime::{bundle::Bundle, modules::Input};
use futures_util::StreamExt;
use geoipd::GeoIpLookup;
use poem::{
    EndpointExt, IntoResponse, Request, Response, Route, Server,
    error::ResponseError,
    get, handler,
    http::StatusCode,
    listener::{TcpListener, UnixListener},
    middleware::{Cors, Tracing},
    post,
    web::{Data, Html, Json, Query},
};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[cfg(feature = "mp3")]
use mp3lame_encoder::{Builder, FlushNoGap};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the TTS HTTP server
    Serve {
        /// Path to speech bundle or directory containing tts.drb
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

        /// Show voice selector in web UI (for multi-voice deployments)
        #[arg(long, env = "MULTI_VOICE")]
        multi_voice: bool,
    },

    /// Interactively debug a text processing bundle
    DebugText {
        /// Path to a text-*.drb bundle file
        bundle_path: PathBuf,
    },
}

type Config = HashMap<String, VoiceConfig>;
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct VoiceConfig {
    name: String,
    language: usize,
    speakers: HashMap<usize, String>,
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
    #[serde(default = "default_pace")]
    pace: f32,
    /// "f32" for 32-bit float WAV, defaults to 16-bit integer
    #[serde(default)]
    sample_format: Option<String>,
}

fn default_pace() -> f32 {
    1.0
}

fn write_wav(samples: &[f32], use_f32: bool) -> (&'static str, Vec<u8>) {
    if use_f32 {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 22050,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let out = Vec::with_capacity(samples.len() * 4 + 44);
        let mut out = std::io::Cursor::new(out);
        let mut writer = hound::WavWriter::new(&mut out, spec).expect("Vec write infallible");
        for &s in samples {
            writer.write_sample(s).expect("Vec write infallible");
        }
        drop(writer);
        ("audio/wav", out.into_inner())
    } else {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 22050,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let out = Vec::with_capacity(samples.len() * 2 + 44);
        let mut out = std::io::Cursor::new(out);
        let mut writer = hound::WavWriter::new(&mut out, spec).expect("Vec write infallible");
        for &s in samples {
            let clamped = s.clamp(-1.0, 1.0);
            let sample = (clamped * i16::MAX as f32) as i16;
            writer.write_sample(sample).expect("Vec write infallible");
        }
        drop(writer);
        ("audio/wav", out.into_inner())
    }
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

#[cfg(feature = "mp3")]
fn convert_to_mp3(samples: &[f32], text: &str) -> anyhow::Result<Vec<u8>> {
    use mp3lame_encoder::{Bitrate, Id3Tag, MonoPcm, Quality};

    const TARGET_SAMPLE_RATE: u32 = 22050;

    // Resample audio to target sample rate
    // let resampled_samples = resample_audio(samples, sample_rate, TARGET_SAMPLE_RATE)?;

    let mut builder =
        Builder::new().ok_or_else(|| anyhow::anyhow!("Failed to create MP3 encoder builder"))?;

    builder
        .set_num_channels(1) // Audio is always mono
        .map_err(|e| anyhow::anyhow!("Failed to set channels: {:?}", e))?;

    builder
        .set_sample_rate(TARGET_SAMPLE_RATE) // Use target sample rate
        .map_err(|e| anyhow::anyhow!("Failed to set sample rate: {:?}", e))?;

    builder
        .set_brate(Bitrate::Kbps128)
        .map_err(|e| anyhow::anyhow!("Failed to set bitrate: {:?}", e))?;

    builder
        .set_quality(Quality::Best)
        .map_err(|e| anyhow::anyhow!("Failed to set quality: {:?}", e))?;
    builder
        .set_id3_tag(Id3Tag {
            title: text.as_bytes(),
            artist: &[],
            album: b"",
            album_art: &[],
            year: b"",
            comment: b"Generated by Divvun TTS",
        })
        .map_err(|e| anyhow::anyhow!("Failed to set ID3 tag: {:?}", e))?;
    let mut encoder = builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build MP3 encoder: {:?}", e))?;

    let mut mp3_buffer = Vec::new();

    // Encode resampled samples
    let input = MonoPcm(&samples);
    mp3_buffer.reserve(mp3lame_encoder::max_required_buffer_size(samples.len()));
    let encoded_size = encoder
        .encode(input, mp3_buffer.spare_capacity_mut())
        .map_err(|e| anyhow::anyhow!("Failed to encode MP3 data: {:?}", e))?;
    unsafe {
        mp3_buffer.set_len(mp3_buffer.len().wrapping_add(encoded_size));
    }

    // Flush remaining data
    let encoded_size = encoder
        .flush::<FlushNoGap>(mp3_buffer.spare_capacity_mut())
        .map_err(|e| anyhow::anyhow!("Failed to flush MP3 encoder: {:?}", e))?;
    unsafe {
        mp3_buffer.set_len(mp3_buffer.len().wrapping_add(encoded_size));
    }

    Ok(mp3_buffer)
}

enum StreamData {
    Text(Vec<String>),
    Audio(Vec<f32>),
}

#[derive(Debug)]
pub enum AppError {
    LanguageNotFound(usize),
    PipelineCreation(String),
    TextProcessing(String),
    SpeechSynthesis(String),
    WavProcessing(String),
    AudioEncoding(String),
    NoOutput,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LanguageNotFound(id) => write!(f, "Language {} not found", id),
            Self::PipelineCreation(e) => write!(f, "Pipeline creation failed: {}", e),
            Self::TextProcessing(e) => write!(f, "Text processing failed: {}", e),
            Self::SpeechSynthesis(e) => write!(f, "Speech synthesis failed: {}", e),
            Self::WavProcessing(e) => write!(f, "WAV processing failed: {}", e),
            Self::AudioEncoding(e) => write!(f, "Audio encoding failed: {}", e),
            Self::NoOutput => write!(f, "No audio output generated"),
        }
    }
}

impl std::error::Error for AppError {}

impl ResponseError for AppError {
    fn status(&self) -> StatusCode {
        match self {
            Self::LanguageNotFound(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn as_response(&self) -> Response {
        if self.status().is_server_error() {
            tracing::error!(error = %self, "Server error");
        } else {
            tracing::warn!(error = %self, "Client error");
        }

        Response::builder()
            .status(self.status())
            .content_type("application/json")
            .body(serde_json::json!({"error": self.to_string()}).to_string())
    }
}

#[handler]
async fn process(
    Data(holder): Data<&Arc<PipelineHolder>>,
    Query(query): Query<ProcessQuery>,
    Json(body): Json<ProcessInput>,
    req: &Request,
) -> Result<Response, AppError> {
    let time_start = std::time::Instant::now();
    let _country = match body.country.as_deref() {
        Some("") => None,
        Some(v) => Some(v.to_string()),
        None => derive_country(req).await,
    };

    let text = holder
        .text
        .get(&query.language)
        .ok_or(AppError::LanguageNotFound(query.language))?;

    let mut speech_pipeline = holder
        .speech
        .create(serde_json::json!({
            "tts":
            {
                "language": query.language,
                "speaker": query.speaker,
                "pace": query.pace,
            }
        }))
        .await
        .map_err(|e| AppError::PipelineCreation(e.to_string()))?;

    let mut config = serde_json::json!({});

    let mut text_pipeline = text
        .create(config)
        .await
        .map_err(|e| AppError::TextProcessing(e.to_string()))?;

    let mut stream = text_pipeline
        .forward(Input::String(body.text.clone()))
        .await;

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
                        let output = match output.try_into_bytes() {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                yield Err(divvun_runtime::modules::Error::msg(format!("Failed to convert output to bytes: {:?}", e)));
                                return;
                            }
                        };
                        tracing::debug!(
                            len = output.len(),
                            header = ?output.get(0..12).map(|h| String::from_utf8_lossy(h).to_string()),
                            "Speech pipeline output"
                        );
                        let mut reader = match hound::WavReader::new(std::io::Cursor::new(output.clone())) {
                            Ok(reader) => reader,
                            Err(e) => {
                                tracing::error!(
                                    len = output.len(),
                                    header_hex = ?output.get(0..44).map(hex::encode),
                                    "Failed to parse WAV: {}", e
                                );
                                yield Err(divvun_runtime::modules::Error::msg(format!("Failed to read WAV data: {}", e)));
                                return;
                            }
                        };
                        let samples = reader.samples::<f32>().collect::<Result<Vec<_>, _>>();
                        let samples = match samples {
                            Ok(samples) => samples,
                            Err(e) => {
                                yield Err(divvun_runtime::modules::Error::msg(e.to_string()));
                                return;
                            }
                        };
                        yield Ok(StreamData::Audio(samples));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Speech pipeline returned error");
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
                return Err(AppError::SpeechSynthesis(e.to_string()));
            }
        }
    }

    if bytes.is_empty() {
        return Err(AppError::NoOutput);
    }

    let use_f32 = query
        .sample_format
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("f32"))
        .unwrap_or(false);

    // Check if client accepts MP3
    let wants_mp3 = req
        .header("Accept")
        .map(|accept| accept.contains("audio/mpeg"))
        .unwrap_or(false);

    let (content_type, output_data) = if wants_mp3 {
        // Convert to MP3 if feature is enabled and client wants it
        #[cfg(feature = "mp3")]
        {
            let mp3_data = convert_to_mp3(&bytes, &body.text)
                .map_err(|e| AppError::AudioEncoding(e.to_string()))?;
            ("audio/mpeg", mp3_data)
        }
        #[cfg(not(feature = "mp3"))]
        {
            write_wav(&bytes, use_f32)
        }
    } else {
        write_wav(&bytes, use_f32)
    };

    let time = time_start.elapsed().as_millis();

    tracing::info!(
        time_ms = time,
        language = query.language,
        speaker = query.speaker,
        format = content_type,
        "generated {} bytes.",
        output_data.len()
    );

    let mut response = Response::builder()
        .header("Content-Type", content_type)
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

    Ok(response.body(output_data))
}

const PAGE: &str = include_str!("../index.html");

#[handler]
async fn process_get(Data(holder): Data<&Arc<PipelineHolder>>) -> impl IntoResponse {
    Html(holder.page.clone()).into_response()
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
    page: String,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(fmt::layer().with_file(true).with_line_number(true))
        .with(filter)
        .init();
}

async fn run() -> anyhow::Result<()> {
    init_tracing();

    let args = Args::parse();

    match args.command {
        Command::Serve {
            bundle_path,
            address,
            config_path,
            maxmind_account_id,
            maxmind_license_key,
            multi_voice,
        } => {
            run_serve(
                bundle_path,
                address,
                config_path,
                maxmind_account_id,
                maxmind_license_key,
                multi_voice,
            )
            .await
        }
        Command::DebugText { bundle_path } => run_debug_text(bundle_path).await,
    }
}

async fn run_debug_text(bundle_path: PathBuf) -> anyhow::Result<()> {
    eprintln!("Loading bundle from {}...", bundle_path.display());
    let bundle = Bundle::from_bundle(&bundle_path).await?;
    eprintln!("Bundle loaded. Type text to process (Ctrl-D to quit).");

    let mut rl = rustyline::DefaultEditor::new()?;

    while let Ok(line) = rl.readline("> ") {
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(&line);

        let mut pipeline = bundle
            .create(serde_json::json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create pipeline: {e}"))?;

        let mut stream = pipeline.forward(Input::String(line)).await;

        while let Some(result) = stream.next().await {
            match result {
                Ok(Input::String(s)) => println!("{s}"),
                Ok(Input::ArrayString(arr)) => {
                    for s in &arr {
                        println!("{s}");
                    }
                }
                Ok(other) => eprintln!("(unexpected output type: {other:?})"),
                Err(e) => eprintln!("Error: {e}"),
            }
        }
    }

    Ok(())
}

async fn run_serve(
    bundle_path: PathBuf,
    address: String,
    config_path: PathBuf,
    maxmind_account_id: Option<String>,
    maxmind_license_key: Option<String>,
    multi_voice: bool,
) -> anyhow::Result<()> {
    let (config_path, speech_bundle_path, bundle_base_path) = if bundle_path.is_dir() {
        // Directory mode: use tts.drb and config.toml from directory
        let config_path = bundle_path.join("config.toml");
        let speech_bundle_path = bundle_path.join("tts.drb");
        (config_path, speech_bundle_path, bundle_path)
    } else {
        // File mode: use provided bundle file and config
        let bundle_base_path = bundle_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        (config_path, bundle_path, bundle_base_path)
    };

    tracing::info!("Loading config from {}", config_path.display());
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    tracing::info!(
        "Loading speech bundle from {}",
        speech_bundle_path.display()
    );
    let speech = Bundle::from_bundle(&speech_bundle_path).await?;

    // Generate language options HTML if multi-voice mode
    let page = if multi_voice {
        let mut language_options = String::new();
        for (code, voice) in &config {
            let speakers_json = serde_json::to_string(&voice.speakers)?;
            language_options.push_str(&format!(
                r#"<option value="{}" data-speakers='{}'>{} ({})</option>"#,
                voice.language, speakers_json, voice.name, code
            ));
        }
        PAGE.replace("%LANGUAGE_OPTIONS%", &language_options)
            .replace("%VOICE_SETTINGS_STYLE%", "")
            .replace("%VERSION%", env!("CARGO_PKG_VERSION"))
    } else {
        PAGE.replace("%LANGUAGE_OPTIONS%", "")
            .replace("%VOICE_SETTINGS_STYLE%", "display: none;")
            .replace("%VERSION%", env!("CARGO_PKG_VERSION"))
    };

    let mut text = HashMap::new();

    for (language, voice_config) in config {
        tracing::info!("Loading text bundle for language {}", language);
        let bundle =
            Bundle::from_bundle(bundle_base_path.join(format!("text-{}.drb", language))).await?;
        text.insert(voice_config.language, bundle);
    }

    let holder = PipelineHolder { speech, text, page };

    let geoip =
        if let (Some(account_id), Some(license_key)) = (maxmind_account_id, maxmind_license_key) {
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
        .with(Cors::default())
        .with(Tracing);

    let address = ListenerAddress::parse(&address)?;

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
