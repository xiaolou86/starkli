use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use colored_json::{ColorMode, Output};
use starknet::{
    accounts::Account,
    core::types::{
        contract::{legacy::LegacyContractClass, CompiledClass, SierraClass},
        BlockId, BlockTag, FieldElement, StarknetError,
    },
    macros::felt,
    providers::{Provider, ProviderError},
};

use crate::{
    account::AccountArgs,
    casm::{CasmArgs, CasmHashSource},
    fee::{FeeArgs, FeeSetting},
    path::ExpandedPathbufParser,
    utils::watch_tx,
    verbosity::VerbosityArgs,
    ProviderArgs,
};

#[derive(Debug, Parser)]
pub struct Declare {
    #[clap(flatten)]
    provider: ProviderArgs,
    #[clap(flatten)]
    account: AccountArgs,
    #[clap(flatten)]
    casm: CasmArgs,
    #[clap(flatten)]
    fee: FeeArgs,
    #[clap(long, help = "Simulate the transaction only")]
    simulate: bool,
    #[clap(long, help = "Provide transaction nonce manually")]
    nonce: Option<FieldElement>,
    #[clap(long, short, help = "Wait for the transaction to confirm")]
    watch: bool,
    #[clap(
        long,
        env = "STARKNET_POLL_INTERVAL",
        default_value = "5000",
        help = "Transaction result poll interval in milliseconds"
    )]
    poll_interval: u64,
    #[clap(
        value_parser = ExpandedPathbufParser,
        help = "Path to contract artifact file"
    )]
    file: PathBuf,
    #[clap(flatten)]
    verbosity: VerbosityArgs,
}

impl Declare {
    pub async fn run(self) -> Result<()> {
        self.verbosity.setup_logging();

        let fee_setting = self.fee.into_setting()?;
        if self.simulate && fee_setting.is_estimate_only() {
            anyhow::bail!("--simulate cannot be used with --estimate-only");
        }

        let provider = Arc::new(self.provider.into_provider()?);

        let account = self.account.into_account(provider.clone()).await?;

        // Workaround for issue:
        //   https://github.com/eqlabs/pathfinder/issues/1208
        let (fee_multiplier_num, fee_multiplier_denom): (FieldElement, FieldElement) =
            if provider.is_rpc() {
                (felt!("5"), felt!("2"))
            } else {
                (felt!("3"), felt!("2"))
            };

        // Working around a deserialization bug in `starknet-rs`:
        //   https://github.com/xJonathanLEI/starknet-rs/issues/392

        #[allow(clippy::redundant_pattern_matching)]
        let (class_hash, declaration_tx_hash) = if let Ok(class) =
            serde_json::from_reader::<_, SierraClass>(std::fs::File::open(&self.file)?)
        {
            // Declaring Cairo 1 class
            let class_hash = class.class_hash()?;

            // TODO: add option to skip checking
            if Self::check_already_declared(&provider, class_hash).await? {
                return Ok(());
            }

            let casm_source = self.casm.into_casm_hash_source(&provider).await?;

            if !fee_setting.is_estimate_only() {
                eprintln!(
                    "Declaring Cairo 1 class: {}",
                    format!("{:#064x}", class_hash).bright_yellow()
                );

                match &casm_source {
                    CasmHashSource::BuiltInCompiler(compiler) => {
                        eprintln!(
                            "Compiling Sierra class to CASM with compiler version {}...",
                            format!("{}", compiler.version()).bright_yellow()
                        );
                    }
                    CasmHashSource::CompilerBinary(compiler) => {
                        eprintln!(
                            "Compiling Sierra class to CASM with compiler binary {}...",
                            format!("{}", compiler.path().display()).bright_yellow()
                        );
                    }
                    CasmHashSource::CasmFile(path) => {
                        eprintln!(
                            "Using a compiled CASM file directly: {}...",
                            format!("{}", path.display()).bright_yellow()
                        );
                    }
                    CasmHashSource::Hash(hash) => {
                        eprintln!(
                            "Using the provided CASM hash: {}...",
                            format!("{:#064x}", hash).bright_yellow()
                        );
                    }
                }
            }

            let casm_class_hash = casm_source.get_casm_hash(&class)?;

            if !fee_setting.is_estimate_only() {
                eprintln!(
                    "CASM class hash: {}",
                    format!("{:#064x}", casm_class_hash).bright_yellow()
                );
            }

            // TODO: make buffer configurable
            let declaration = account.declare(Arc::new(class.flatten()?), casm_class_hash);

            let max_fee = match fee_setting {
                FeeSetting::Manual(fee) => fee,
                FeeSetting::EstimateOnly | FeeSetting::None => {
                    let estimated_fee = declaration.estimate_fee().await?.overall_fee;

                    if fee_setting.is_estimate_only() {
                        println!(
                            "{} ETH",
                            format!("{}", estimated_fee.to_big_decimal(18)).bright_yellow(),
                        );
                        return Ok(());
                    }

                    // TODO: make buffer configurable
                    (estimated_fee * fee_multiplier_num).floor_div(fee_multiplier_denom)
                }
            };

            let declaration = match self.nonce {
                Some(nonce) => declaration.nonce(nonce),
                None => declaration,
            };
            let declaration = declaration.max_fee(max_fee);

            if self.simulate {
                let simulation = declaration.simulate(false, false).await?;
                let simulation_json = serde_json::to_value(simulation)?;

                let simulation_json = colored_json::to_colored_json(
                    &simulation_json,
                    ColorMode::Auto(Output::StdOut),
                )?;
                println!("{simulation_json}");
                return Ok(());
            }

            (class_hash, declaration.send().await?.transaction_hash)
        } else if let Ok(_) =
            serde_json::from_reader::<_, CompiledClass>(std::fs::File::open(&self.file)?)
        {
            // TODO: add more helpful instructions to fix this
            anyhow::bail!("unexpected CASM class");
        } else if let Ok(class) =
            serde_json::from_reader::<_, LegacyContractClass>(std::fs::File::open(self.file)?)
        {
            // Declaring Cairo 0 class
            let class_hash = class.class_hash()?;

            // TODO: add option to skip checking
            if Self::check_already_declared(&provider, class_hash).await? {
                return Ok(());
            }

            if !fee_setting.is_estimate_only() {
                eprintln!(
                    "Declaring Cairo 0 (deprecated) class: {}",
                    format!("{:#064x}", class_hash).bright_yellow()
                );
            }

            // TODO: make buffer configurable
            let declaration = account.declare_legacy(Arc::new(class));

            let max_fee = match fee_setting {
                FeeSetting::Manual(fee) => fee,
                FeeSetting::EstimateOnly | FeeSetting::None => {
                    let estimated_fee = declaration.estimate_fee().await?.overall_fee;

                    if fee_setting.is_estimate_only() {
                        println!(
                            "{} ETH",
                            format!("{}", estimated_fee.to_big_decimal(18)).bright_yellow(),
                        );
                        return Ok(());
                    }

                    // TODO: make buffer configurable
                    (estimated_fee * fee_multiplier_num).floor_div(fee_multiplier_denom)
                }
            };

            let declaration = match self.nonce {
                Some(nonce) => declaration.nonce(nonce),
                None => declaration,
            };
            let declaration = declaration.max_fee(max_fee);

            if self.simulate {
                let simulation = declaration.simulate(false, false).await?;
                let simulation_json = serde_json::to_value(simulation)?;

                let simulation_json = colored_json::to_colored_json(
                    &simulation_json,
                    ColorMode::Auto(Output::StdOut),
                )?;
                println!("{simulation_json}");
                return Ok(());
            }

            (class_hash, declaration.send().await?.transaction_hash)
        } else {
            anyhow::bail!("failed to parse contract artifact");
        };

        eprintln!(
            "Contract declaration transaction: {}",
            format!("{:#064x}", declaration_tx_hash).bright_yellow()
        );

        if self.watch {
            eprintln!(
                "Waiting for transaction {} to confirm...",
                format!("{:#064x}", declaration_tx_hash).bright_yellow(),
            );
            watch_tx(
                &provider,
                declaration_tx_hash,
                Duration::from_millis(self.poll_interval),
            )
            .await?;
        }

        eprintln!("Class hash declared:");

        // Only the class hash goes to stdout so this can be easily scripted
        println!("{}", format!("{:#064x}", class_hash).bright_yellow());

        Ok(())
    }

    async fn check_already_declared<P>(provider: P, class_hash: FieldElement) -> Result<bool>
    where
        P: Provider,
    {
        match provider
            .get_class(BlockId::Tag(BlockTag::Pending), class_hash)
            .await
        {
            Ok(_) => {
                eprintln!("Not declaring class as it's already declared. Class hash:");
                println!("{}", format!("{:#064x}", class_hash).bright_yellow());

                Ok(true)
            }
            Err(ProviderError::StarknetError(StarknetError::ClassHashNotFound)) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }
}
