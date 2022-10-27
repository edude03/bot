use std::env::temp_dir;

use anyhow::anyhow;
use axum::routing::get;
use axum::Router;
use clap::Parser;
use reqwest::multipart;
use reqwest::{Body, Url};
use serde::Deserialize;
use tap::Tap;
use teloxide::dispatching::update_listeners::webhooks::{axum_to_router, Options};
use teloxide::dispatching::update_listeners::UpdateListener;
use teloxide::net::Download;
use teloxide::types::File as TelegramFile;
use teloxide::RequestError;
use teloxide::{dispatching::update_listeners::webhooks, prelude::*};
use tempfile::{tempfile, NamedTempFile, TempDir};
use tokio::fs::File;
use tokio_util::codec::{BytesCodec, FramedRead};
use tracing::{debug, error, info, instrument, span, Instrument, Level};
#[derive(Parser)]
struct Config {
    #[clap(env)]
    telegram_token: String,
    #[clap(env)]
    port: u16,
    #[clap(env)]
    external_url: Url,
}

#[instrument(skip_all, fields(message_id = ?msg.id, user = ?msg.from()), err(Debug))]
async fn handle_message(bot: Bot, msg: Message) -> Result<(), RequestError> {
    let file_id = if let Some(file) = msg.audio() {
        Some(file.file.id.clone())
    } else if let Some(file) = msg.voice() {
        Some(file.file.id.clone())
    } else {
        None
    };

    if let Some(id) = file_id {
        bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
            .await;
        let resp = transcribe(&bot, id)
            .await
            .map_err(|e| {
                error!("it failed {}", e);
                format!("It failed")
            })
            .unwrap();

        bot.send_message(msg.chat.id, resp.text).await?;
        Ok(())
    } else {
        bot.send_message(msg.chat.id, "failed").await?;
        Ok(())
    }
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse();
    tracing_subscriber::fmt::init();

    info!("Starting Bot");
    let bot = Bot::new(config.telegram_token);
    let addr = ([127, 0, 0, 1], config.port).into();

    let health = Router::new().route("/healthz", get(health));
    // let (mut update_listener, stop_flag, app) = axum_to_router(
    //     bot.clone(),
    //     webhooks::Options::new(addr, config.external_url),
    // )
    // .await?;
    // let stop_token = update_listener.stop_token();

    tokio::spawn(async move {
        axum::Server::bind(&addr)
            .serve(health.into_make_service())
            // .with_graceful_shutdown(stop_flag)
            .await
            // .map_err(|err| {
            //     stop_token.stop();
            //     err
            // })
            .expect("Axum server error");
    });

    // teloxide::repl_with_listener(bot, handle_message, update_listener).await;

    teloxide::repl(bot, handle_message).await;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct Segment {
    id: f32,
    seek: f32,
    start: f32,
    end: f32,
    text: String,
    tokens: Vec<f32>,
    temperature: f32,
    avg_logprob: f64,
    compression_ratio: f64,
    no_speech_prob: f64,
}

#[derive(Debug, Deserialize)]
struct TranscriptionResult {
    text: String,
    //segments: Vec<Segment>,
}

async fn transcribe(bot: &Bot, file_id: String) -> Result<TranscriptionResult, anyhow::Error> {
    info!("Getting file");
    let audio_file: TelegramFile = bot
        .get_file(file_id)
        .send()
        .instrument(span!(Level::INFO, "Getting file details"))
        .await
        .map_err(|err| anyhow!("Failed to download the file: {err}"))?;

    info!("Creating temp file");

    let tempdir = temp_dir().tap_mut(|d| {
        d.push("test.ogg");
        info!(file_path = %d.display(), "Temp file created");
    });

    let mut f = File::create(tempdir.clone()).await?;

    bot.download_file(&audio_file.path, &mut f)
        .instrument(span!(Level::INFO, "Downloading file"))
        .await?;

    let read = File::open(tempdir).await?;

    let stream = FramedRead::new(read, BytesCodec::new());
    let file_body = Body::wrap_stream(stream);

    //make form part of file
    let some_file = multipart::Part::stream(file_body)
        .file_name("audio_file")
        .mime_str("audio/ogg")?;

    //create the multipart form
    let form = multipart::Form::new().part("audio_file", some_file);
    let client = reqwest::Client::new();

    let resp = client
        .post("http://asr.default.svc.cluster.local/asr")
        .multipart(form)
        .send()
        .instrument(span!(Level::INFO, "Making request to service"))
        .await?;

    if resp.status().is_client_error() || resp.status().is_server_error() {
        let err = resp.text().await?;
        anyhow::bail!("Failed for some reason {err}")
    } else {
        resp.json::<TranscriptionResult>()
            .await
            .map_err(|e| anyhow!("Failed: {}", e))
    }
}
