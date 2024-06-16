use std::sync::Arc;

use divvun_runtime::{modules::Input, Bundle};
use poem::{handler, listener::TcpListener, post, web::{Data, Json}, EndpointExt, IntoResponse, Route, Server};

#[derive(serde::Deserialize)]
struct ProcessInput {
    text: String,
}

#[handler]
async fn process(
    Data(bundle): Data<&Arc<Bundle>>,
    Json(body): Json<ProcessInput>
) -> impl IntoResponse {
    let output = match bundle.run_pipeline(Input::String(body.text)).await {
        Ok(output) => output,
        Err(e) => {
            tracing::error!("{:?}", e);
            return Json(serde_json::json!({
                "error": e.to_string()
            })).into_response();
        }
    };

    let output = output.try_into_bytes().unwrap();
    output.into_response()
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
    {
        let output = bundle.run_pipeline(Input::String("Hello".into())).await?;
        let _bytes = output.try_into_bytes()?;
    }
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
    
    let app = Route::new().at("/", post(process)).data(bundle);

    Server::new(TcpListener::bind((host, port)))
      .run(app)
      .await?;

    Ok(())
}