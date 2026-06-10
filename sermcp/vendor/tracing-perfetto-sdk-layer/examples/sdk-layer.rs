use std::{fs, thread, time};

use tracing_perfetto_sdk_schema as schema;
use tracing_perfetto_sdk_schema::trace_config;

fn trace_config() -> schema::TraceConfig {
    // In a real app, one would read this from a config file or similar.
    schema::TraceConfig {
        buffers: vec![trace_config::BufferConfig {
            size_kb: Some(1024),
            ..Default::default()
        }],
        data_sources: vec![trace_config::DataSource {
            config: Some(schema::DataSourceConfig {
                name: Some("rust_tracing".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn main() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let out_file = "sdk-layer-example.pftrace";
    let layer = tracing_perfetto_sdk_layer::SdkLayer::from_config(
        trace_config(),
        Some(fs::File::create(out_file)?),
    )
    .build()?;

    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(foo = "baz", int = 7, "hello trace!");

        let span = tracing::info_span!("hi", foo = "bar", int = 3);
        let guard = span.enter();
        thread::sleep(time::Duration::from_secs(1));
        drop(guard);
    });

    let trace = schema::Trace::decode(&*fs::read(out_file)?)?;
    let trace_str = serde_yaml::to_string(&trace)?;
    eprintln!("{}", trace_str);

    Ok(())
}
