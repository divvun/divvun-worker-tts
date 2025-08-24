use std::{collections::HashMap, fmt::Display, path::PathBuf, sync::Arc};

use clap::Parser;
use divvun_runtime::{Bundle, modules::Input};
use futures_util::StreamExt;
use poem::{
    EndpointExt, IntoResponse, Response, Route, Server, get, handler,
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
}

#[derive(serde::Deserialize)]
struct ProcessQuery {
    #[serde(default)]
    speaker: usize,
    #[serde(default)]
    language: usize,
}

#[handler]
async fn process(
    Data(holder): Data<&Arc<PipelineHolder>>,
    Query(query): Query<ProcessQuery>,
    Json(body): Json<ProcessInput>,
) -> impl IntoResponse {
    let time_start = std::time::Instant::now();

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

    let mut text_pipeline = match text.create(serde_json::json!({})).await {
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
                        yield Ok(samples);
                    }
                    Err(e) => {
                        yield Err(e);
                    }
                }
            }
        }
    });

    let mut bytes = Vec::new();
    while let Some(output) = stream.next().await {
        match output {
            Ok(output) => bytes.extend(output),
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

    Response::builder()
        .header("Content-Type", "audio/wav")
        .body(out)
        .into_response()
}

#[handler]
async fn text_process(
    Data(holder): Data<&Arc<PipelineHolder>>,
    Query(query): Query<ProcessQuery>,
    Json(body): Json<ProcessInput>,
) -> impl IntoResponse {
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

    let mut text_pipeline = match text.create(serde_json::json!({})).await {
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
            yield Ok(output);
        };
    });

    let mut out = String::new();
    while let Some(output) = stream.next().await {
        match output {
            Ok(output) => {
                out = output.try_into_string().unwrap();
            }
            Err(e) => {
                return Json(serde_json::json!({
                    "error": e.to_string()
                }))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
            }
        }
    }

    Json(serde_json::json!({
        "text": out
    }))
    .into_response()
}

const PAGE: &str = r#"
<!doctype html>
<html>
<head>
<title>Divvun TTS</title>
<meta charset="utf-8">
<style>
.container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 16px;
}

</style>
</head>
<body>
<div class="container">
<textarea class="text" autofocus></textarea>
<button class="doit">
Speak
</button>
<audio controls class="audio"></audio>
<p class="output"></p>
<script>

async function speak(text) {
    const audio = document.querySelector(".audio")

    const response = await fetch(location.href, {
        method: "POST",
        headers: {"Content-Type": "application/json" },
        body: JSON.stringify({ text }),
    })

    const buffer = await response.arrayBuffer()
    const blob = new Blob([buffer], { type: "audio/wav" });
    audio.src = URL.createObjectURL(blob);
    audio.play()
}

async function generateText(text) {
    const url = new URL(location.href)
    const response = await fetch(url.origin + url.pathname + "/text" + url.search, {
        method: "POST",
        headers: {"Content-Type": "application/json" },
        body: JSON.stringify({ text }),
    })

    const data = await response.json()
    console.log(data)
    document.querySelector(".output").innerText = data.text
}

const submit = async (e) => {
    e.preventDefault()
    const node = document.querySelector(".doit")

    try {
        const text = document.querySelector(".text").value.trim()

        node.innerHTML = "Generating..."
        await Promise.all([
            generateText(text),
            speak(text),
        ])
    } catch (e) {
        console.error(e)
        document.querySelector(".output").innerText = "Error: " + e.message
    } finally {
        node.innerHTML = "Speak"
    }
}

document.querySelector(".doit").addEventListener("click", submit);

// When Cmd+Enter or Ctrl+Enter is pressed, submit the form
document.querySelector(".text").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        submit(e);
    }
})

</script>
</div>
</body>
</html>
"#;

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

    let app = Route::new()
        .at("/", post(process).get(process_get))
        .at("/text", post(text_process))
        .at("/health", get(health_get))
        .data(Arc::new(holder))
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
