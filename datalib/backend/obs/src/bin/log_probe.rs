//! A step that logs one line through `datalib_obs::init`, as every built-in
//! step does, and exits: what the runner files a built-in step's line as.

fn main() -> anyhow::Result<()> {
    let args = datalib_obs::ObsArgs {
        log_level: Some("info".to_string()),
        ..Default::default()
    };
    let _guard = datalib_obs::init(&args, "log_probe")?;
    tracing::info!("a line that stamps itself");
    Ok(())
}
