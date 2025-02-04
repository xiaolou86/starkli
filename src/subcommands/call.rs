use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use starknet::{
    core::types::{BlockId, BlockTag, FunctionCall},
    providers::Provider,
};

use crate::{
    address_book::AddressBookResolver, decode::FeltDecoder, verbosity::VerbosityArgs, ProviderArgs,
};

#[derive(Debug, Parser)]
pub struct Call {
    #[clap(flatten)]
    provider: ProviderArgs,
    #[clap(help = "Contract address")]
    contract_address: String,
    #[clap(help = "Name of the function being called")]
    selector: String,
    #[clap(help = "Raw function call arguments")]
    calldata: Vec<String>,
    #[clap(flatten)]
    verbosity: VerbosityArgs,
}

impl Call {
    pub async fn run(self) -> Result<()> {
        self.verbosity.setup_logging();

        let provider = Arc::new(self.provider.into_provider()?);
        let felt_decoder = FeltDecoder::new(AddressBookResolver::new(provider.clone()));

        let contract_address = felt_decoder
            .decode_single_with_addr_fallback(&self.contract_address)
            .await?;
        let selector = felt_decoder
            .decode_single_with_selector_fallback(&self.selector)
            .await?;

        let mut calldata = vec![];
        for element in self.calldata.iter() {
            calldata.append(&mut felt_decoder.decode(element).await?);
        }

        let result = provider
            .call(
                FunctionCall {
                    contract_address,
                    entry_point_selector: selector,
                    calldata,
                },
                BlockId::Tag(BlockTag::Pending),
            )
            .await?;

        if result.is_empty() {
            println!("[]");
        } else {
            println!("[");

            for (ind_element, element) in result.iter().enumerate() {
                println!(
                    "    \"{:#064x}\"{}",
                    element,
                    if ind_element == result.len() - 1 {
                        ""
                    } else {
                        ","
                    }
                );
            }

            println!("]");
        }

        Ok(())
    }
}
