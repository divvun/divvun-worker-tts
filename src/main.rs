use std::{ops::Deref, sync::Arc};

use clap::Parser;
use divvun_runtime::{Bundle, ast::PipelineHandle, modules::Input};
use futures_util::{FutureExt, StreamExt, TryStreamExt as _};
use poem::{
    handler, listener::TcpListener, middleware::Cors, post, web::{Data, Html, Json, Path, Query}, EndpointExt, IntoResponse, Response, Route, Server
};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Config {
    /// Path to text processor bundle
    #[arg(required = true)]
    text_processor_bundle_path: String,

    /// Path to speech bundle
    #[arg(required = true)]
    speech_bundle_path: String,

    /// Port to listen on
    #[arg(short, long, env = "PORT", default_value = "4000")]
    port: u16,

    /// Host address to bind to
    #[arg(short, long, env = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// Speaker to use
    #[arg(short, long, env = "SPEAKER", default_value = "0")]
    speaker: i32,

    /// Language to use
    #[arg(short, long, env = "LANGUAGE", default_value = "0")]
    language: i32,
}

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
}

#[derive(Debug, Clone, Copy)]
struct SpeakerId(i32);

#[derive(Debug, Clone, Copy)]
struct LanguageId(i32);

#[handler]
async fn process(
    Data(mut speech_pipeline): Data<&Arc<Mutex<SpeechPipeline>>>,
    Data(mut text_pipeline): Data<&Arc<Mutex<TextPipeline>>>,
    Json(body): Json<ProcessInput>,
) -> impl IntoResponse {
    let mut speech_pipeline_guard = speech_pipeline.lock().await;
    let mut text_pipeline_guard = text_pipeline.lock().await;

    let speech_pipeline = &mut speech_pipeline_guard.pipeline;
    let text_pipeline = &mut text_pipeline_guard.pipeline;

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

    let out = out.into_inner();
    tracing::info!("Generated {} bytes.", out.len());

    Response::builder()
        .header("Content-Type", "audio/wav")
        .body(out)
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
<textarea class="text">

</textarea>
<button class="doit">
Speak
</button>
<audio controls class="audio"></audio>
<script>
document.querySelector(".doit").addEventListener("click", async (e) => {
    const node = document.querySelector(".doit")
    const audio = document.querySelector(".audio")
    e.preventDefault()
    const text = document.querySelector(".text").value.trim()

    node.innerHTML = "Generating..."
    
    const response = await fetch(location.href, {
        method: "POST",
        headers: {"Content-Type": "application/json" },
        body: JSON.stringify({ text }),
    })

    node.innerHTML = "Speak"

    const buffer = await response.arrayBuffer()
    const blob = new Blob([buffer], { type: "audio/wav" });
    audio.src = URL.createObjectURL(blob);
    audio.play()
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Ok(run().await?)
}

struct TextPipeline {
    pipeline: PipelineHandle,
}

impl Deref for TextPipeline {
    type Target = PipelineHandle;

    fn deref(&self) -> &Self::Target {
        &self.pipeline
    }
}

struct SpeechPipeline {
    pipeline: PipelineHandle,
}

impl Deref for SpeechPipeline {
    type Target = PipelineHandle;

    fn deref(&self) -> &Self::Target {
        &self.pipeline
    }
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::parse();

    let speech_bundle = Arc::new(Bundle::from_bundle(config.speech_bundle_path)?);
    let speech_pipeline = match speech_bundle
        .create(serde_json::json!({
            "speaker": config.speaker,
            "language": config.language
        }))
        .await
    {
        Ok(pipeline) => pipeline,
        Err(e) => {
            tracing::error!("{:?}", e);
            return Err(e.into());
        }
    };

    let text_bundle = Arc::new(Bundle::from_bundle(config.text_processor_bundle_path)?);
    let text_pipeline = match text_bundle.create(serde_json::json!({})).await {
        Ok(pipeline) => pipeline,
        Err(e) => {
            tracing::error!("{:?}", e);
            return Err(e.into());
        }
    };

    let app = Route::new()
        .at("/", post(process).get(process_get))
        .data(Arc::new(Mutex::new(SpeechPipeline {
            pipeline: speech_pipeline,
        })))
        .data(Arc::new(Mutex::new(TextPipeline {
            pipeline: text_pipeline,
        })))
        .with(Cors::default());

    Server::new(TcpListener::bind((config.host, config.port)))
        .run(app)
        .await?;

    Ok(())
}
