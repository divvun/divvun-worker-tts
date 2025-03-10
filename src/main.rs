use std::sync::Arc;

use clap::Parser;
use divvun_runtime::{modules::Input, Bundle};
use poem::{
    handler,
    listener::TcpListener,
    middleware::Cors,
    post,
    web::{Data, Html, Json, Path, Query},
    EndpointExt, IntoResponse, Route, Server,
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Config {
    /// Path to the DRB bundle file
    #[arg(required = true)]
    bundle_path: String,

    /// Port to listen on
    #[arg(short, long, env = "PORT", default_value = "4000")]
    port: u16,

    /// Host address to bind to
    #[arg(short, long, env = "HOST", default_value = "127.0.0.1")]
    host: String,

    /// Speaker to use
    #[arg(short, long, env = "SPEAKER", default_value = "0")]
    speaker: i32,
}

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
}

#[derive(serde::Deserialize)]
struct QueryParams {
    #[serde(default)]
    speaker: Option<i32>,
}

#[derive(Debug, Clone, Copy)]
struct SpeakerId(i32);

#[handler]
async fn process(
    Data(bundle): Data<&Arc<Bundle>>,
    Data(SpeakerId(speaker)): Data<&SpeakerId>,
    Json(body): Json<ProcessInput>,
) -> impl IntoResponse {
    let output = match bundle
        .run_pipeline(
            Input::String(body.text),
            Arc::new(serde_json::json!({"speaker": speaker})),
        )
        .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::error!("{:?}", e);
            return Json(serde_json::json!({
                "error": e.to_string()
            }))
            .into_response();
        }
    };

    let output = output.try_into_bytes().unwrap();
    output.into_response()
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

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::parse();
    
    tracing::info!("Starting pipeline from path: {}", config.bundle_path);
    std::env::set_var("PYTHONHOME", std::env::current_dir().unwrap());

    let bundle = Arc::new(Bundle::from_bundle(config.bundle_path)?);
    tracing::info!("Pipeline ready");

    let app = Route::new()
        .at("/", post(process).get(process_get))
        .data(bundle)
        .data(SpeakerId(config.speaker))
        .with(Cors::default());

    Server::new(TcpListener::bind((config.host, config.port)))
        .run(app)
        .await?;

    Ok(())
}
