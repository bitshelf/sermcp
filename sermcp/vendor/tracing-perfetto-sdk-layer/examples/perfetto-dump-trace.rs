use std::io;

use tracing_perfetto_sdk_schema as schema;

fn main() -> anyhow::Result<()> {
    use std::io::Read as _;

    use prost::Message as _;

    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf)?;
    let trace = schema::Trace::decode(buf.as_slice())?;
    serde_yaml::to_writer(io::stdout(), &trace)?;

    Ok(())
}
