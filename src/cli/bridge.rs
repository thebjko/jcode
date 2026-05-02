use anyhow::{Result, bail};

use super::args::BridgeCommand;

pub async fn run_bridge_command(command: BridgeCommand) -> Result<()> {
    match command {
        BridgeCommand::Serve { .. } => {
            bail!(
                "`jcode bridge serve` is planned for private-network transport but is not implemented yet"
            )
        }
        BridgeCommand::Dial { .. } => {
            bail!(
                "`jcode bridge dial` is planned for private-network transport but is not implemented yet"
            )
        }
    }
}
