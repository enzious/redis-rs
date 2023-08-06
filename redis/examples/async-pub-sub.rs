use futures_util::StreamExt;
use redis::AsyncCommands;

#[tokio::main]
async fn main() -> redis::RedisResult<()> {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut publish_conn = client.get_async_connection().await?;
    let mut pubsub = client.get_async_connection().await?.into_pubsub();

    pubsub.subscribe("wavephone").await?;
    publish_conn.publish("wavephone", "banana").await?;

    let pubsub_msg: String = pubsub.next().await.unwrap().unwrap().get_payload()?;
    assert_eq!(&pubsub_msg, "banana");

    Ok(())
}
