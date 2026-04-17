use clap::Args;
use colored::Colorize;
use serde::Serialize;

use crate::shared::reminder::CONDENSED_REMINDER;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct HelloAgentArgs {
    /// Output format override (defaults to human)
    #[arg(long, short = 'f')]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Serialize)]
struct HelloAgentOutput {
    instruction: String,
}

pub async fn exec(_args: HelloAgentArgs, format: OutputFormat) -> anyhow::Result<()> {
    match format {
        OutputFormat::Plain => {
            print!("{}", CONDENSED_REMINDER);
        }
        OutputFormat::Json => {
            let output = HelloAgentOutput {
                instruction: CONDENSED_REMINDER.to_string(),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&output)
                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
            );
        }
        OutputFormat::Human => {
            println!("{}\n", "1up Agent Instructions".bold().underline());
            print!("{}", CONDENSED_REMINDER);
        }
    }
    Ok(())
}
