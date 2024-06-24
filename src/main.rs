use std::sync::Arc;

use divvun_runtime::{modules::Input, Bundle};
use poem::{
    handler,
    listener::TcpListener,
    middleware::Cors,
    post,
    web::{Data, Html, Json, Path},
    EndpointExt, IntoResponse, Route, Server,
};

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
}

#[handler]
async fn process(
    Data(bundle): Data<&Arc<Bundle>>,
    Json(body): Json<ProcessInput>,
    Path(speaker): Path<Option<u32>>,
) -> impl IntoResponse {
    let speaker = speaker.unwrap_or(0);
    let output = match bundle
        .run_pipeline(
            Input::String(body.text),
            serde_json::json!({"speaker": speaker}),
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
    
    const response = await fetch("/", {
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

    let file_path = match std::env::args().skip(1).next() {
        Some(file_path) => file_path,
        None => {
            tracing::error!("No bundle path provided");
            return Err(anyhow::anyhow!("No bundle path provided").into());
        }
    };

    tracing::info!("Starting pipeline from path: {file_path}");
    std::env::set_var("PYTHONHOME", std::env::current_dir().unwrap());

    let bundle = Arc::new(Bundle::from_bundle(file_path)?);
    tracing::info!("Pipeline ready");

    let port = match std::env::var("PORT").ok().map(|x| x.parse::<u16>()) {
        Some(Ok(port)) => port,
        Some(Err(e)) => {
            return Err(e.into());
        }
        None => 4000,
    };

    let host = match std::env::var("HOST").ok() {
        Some(host) => host,
        None => "127.0.0.1".to_string(),
    };

    let app = Route::new()
        .at("/:speaker?", post(process).get(process_get))
        .data(bundle)
        .with(Cors::default());

    Server::new(TcpListener::bind((host, port)))
        .run(app)
        .await?;

    Ok(())
}
