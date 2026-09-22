//! Disposable single-attempt probe. Plaintext is restricted to loopback.
use rustymail_core::{Address, config::Config};
use rustymail_server::relay::{RelayClient, RelayMessage};
use rustymail_store::QueueBody;
use std::{
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, config, source, recipient, mode] = args.as_slice() else {
        return Err("usage: m42_attempt CONFIG SOURCE RECIPIENT plain|tls".into());
    };
    let config = Config::load(config)?;
    let client = match mode.as_str() {
        "plain" => RelayClient::plaintext_lab(
            format!("127.0.0.1:{}", config.relay.port).parse()?,
            &config,
        )?,
        "tls" => RelayClient::load(&config).await?,
        _ => return Err("invalid mode".into()),
    };
    let reader = File::open(source)?;
    let message = RelayMessage {
        sender: None,
        recipient: Address::parse(recipient)?,
        body: QueueBody::EightBitMime,
        stored_size: reader.metadata()?.len(),
        omit_prefix: String::new(),
    };
    let marked = Arc::new(AtomicBool::new(false));
    let flag = marked.clone();
    let result = client
        .attempt(message, reader, move || async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    println!(
        "{}",
        serde_json::json!({"result":result,"body_marked":marked.load(Ordering::SeqCst)})
    );
    Ok(())
}
