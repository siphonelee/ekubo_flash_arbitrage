use color_eyre::eyre::{bail, Result};
use starknet_core::types::Felt;

use crate::ekubo::apis::configuration::Configuration;
use crate::ekubo::models::quotes::Quotes;
use crate::ekubo::models::quote_response::QuoteResponse;
use crate::ekubo::apis::default_api::quote_amount_token_other_token_get;

pub struct Client {
    configuration: Configuration,
}

impl Client {
    #[inline]
    pub fn new(base_path: String, user_agent: String) -> Self {
        let configuration = Configuration {
            base_path,
            user_agent: Some(user_agent),
            client: reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: None,
        };
        Self { configuration }
    }

    #[inline]
    pub async fn quotes(&self, amount: Felt, token: &str, other_token: &str, max_splits: u8, max_hops: u8) -> Result<Quotes> {
        match quote_amount_token_other_token_get(&self.configuration,
                                    &amount.to_string(),
                                            token,
                                            other_token,
                                Some(max_splits.into()),
                                    Some(max_hops.into())).await?
        {
            QuoteResponse::Quotes(q) => Ok(q),
            _ => bail!("quote should return a few Quotes when params are provided"),
        }
    }
}

