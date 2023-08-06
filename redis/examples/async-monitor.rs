use futures_util::StreamExt as _;
use redis::{AsyncCommands, Value};

#[tokio::main]
async fn main() -> redis::RedisResult<()> {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut publish_conn = client.get_async_connection().await?;
    let mut monitor = client.get_async_connection().await?.into_monitor();

    monitor.monitor().await?;

    publish_conn.set("key", b"value").await?;

    let _ = monitor.next().await;

    while let Some(Ok(Value::Status(msg))) = monitor.next().await {
        println!("{}", msg);
    }

    Ok(())
}
