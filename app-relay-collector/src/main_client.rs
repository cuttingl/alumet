use alumet::agent::{static_plugins, AgentBuilder};

use env_logger::Env;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    log::info!("Starting ALUMET relay agent v{VERSION}");

    // Load the collector plugin, and the RAPL one to get some input.
    let plugins = static_plugins![plugin_relay::client::RelayClientPlugin, plugin_rapl::RaplPlugin];

    // Read the config file.
    let config_path = std::path::Path::new("alumet-config.toml");
    let file_content = std::fs::read_to_string(config_path).unwrap_or("".to_owned()); //.expect("failed to read file");
    let config: toml::Table = file_content.parse().unwrap();

    // Start the collector
    let agent = AgentBuilder::new(plugins, config).build();
    let mut pipeline = agent.start();

    // Keep the pipeline running until the app closes.
    pipeline.wait_for_all();
    log::info!("ALUMET relay agent has stopped.");
}
