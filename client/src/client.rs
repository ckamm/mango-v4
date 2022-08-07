use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anchor_client::{ClientError, Cluster, Program};

use anchor_lang::__private::bytemuck;
use anchor_lang::prelude::System;
use anchor_lang::Id;
use anchor_spl::associated_token::get_associated_token_address;
use anchor_spl::token::Token;

use bincode::Options;
use fixed::types::I80F48;
use itertools::Itertools;
use mango_v4::instructions::{Serum3OrderType, Serum3SelfTradeBehavior, Serum3Side};
use mango_v4::state::{Bank, Group, MangoAccountValue, Serum3MarketIndex, TokenIndex};

use solana_client::nonblocking::rpc_client::RpcClient as RpcClientAsync;
use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::keypair;

use crate::account_fetcher::*;
use crate::context::{MangoGroupContext, Serum3MarketContext, TokenContext};
use crate::gpa::fetch_mango_accounts;
use crate::jupiter;
use crate::util::MyClone;

use anyhow::Context;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::sysvar;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signer::Signer};

// very close to anchor_client::Client, which unfortunately has no accessors or Clone
#[derive(Clone, Debug)]
pub struct Client {
    pub cluster: Cluster,
    pub fee_payer: Arc<Keypair>,
    pub commitment: CommitmentConfig,
    pub timeout: Option<Duration>,
}

impl Client {
    pub fn new(
        cluster: Cluster,
        commitment: CommitmentConfig,
        fee_payer: &Keypair,
        timeout: Option<Duration>,
    ) -> Self {
        Self {
            cluster,
            fee_payer: Arc::new(fee_payer.clone()),
            commitment,
            timeout,
        }
    }

    pub fn anchor_client(&self) -> anchor_client::Client {
        anchor_client::Client::new_with_options(
            self.cluster.clone(),
            Rc::new((*self.fee_payer).clone()),
            self.commitment,
        )
    }

    pub fn rpc(&self) -> RpcClient {
        let url = self.cluster.url().to_string();
        if let Some(timeout) = self.timeout.as_ref() {
            RpcClient::new_with_timeout_and_commitment(url, *timeout, self.commitment)
        } else {
            RpcClient::new_with_commitment(url, self.commitment)
        }
    }

    pub fn rpc_async(&self) -> RpcClientAsync {
        let url = self.cluster.url().to_string();
        if let Some(timeout) = self.timeout.as_ref() {
            RpcClientAsync::new_with_timeout_and_commitment(url, *timeout, self.commitment)
        } else {
            RpcClientAsync::new_with_commitment(url, self.commitment)
        }
    }
}

// todo: might want to integrate geyser, websockets, or simple http polling for keeping data fresh
pub struct MangoClient {
    pub client: Client,

    // todo: possibly this object should have cache-functions, so there can be one getMultipleAccounts
    // call to refresh banks etc -- if it's backed by websockets, these could just do nothing
    pub account_fetcher: Arc<dyn AccountFetcher>,

    pub owner: Keypair,
    pub mango_account_address: Pubkey,

    pub context: MangoGroupContext,

    // Since MangoClient currently provides a blocking interface, we'd prefer to use reqwest::blocking::Client
    // but that doesn't work inside async contexts. Hence we use the async reqwest Client instead and use
    // a manual runtime to bridge into async code from both sync and async contexts.
    // That doesn't work perfectly, see MangoClient::invoke().
    pub http_client: reqwest::Client,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for MangoClient {
    fn drop(&mut self) {
        self.runtime.take().expect("runtime").shutdown_background();
    }
}

// TODO: add retry framework for sending tx and rpc calls
// 1/ this works right now, but I think mid-term the MangoClient will want to interact with multiple mango accounts
// -- then we should probably specify accounts by owner+account_num / or pubkey
// 2/ pubkey, can be both owned, but also delegated accouns

impl MangoClient {
    pub fn group_for_admin(admin: Pubkey, num: u32) -> Pubkey {
        Pubkey::find_program_address(
            &["Group".as_ref(), admin.as_ref(), num.to_le_bytes().as_ref()],
            &mango_v4::ID,
        )
        .0
    }

    pub fn find_accounts(
        client: &Client,
        group: Pubkey,
        owner: &Keypair,
    ) -> anyhow::Result<Vec<(Pubkey, MangoAccountValue)>> {
        let program = client.anchor_client().program(mango_v4::ID);
        fetch_mango_accounts(&program, group, owner.pubkey()).map_err(Into::into)
    }

    pub fn find_or_create_account(
        client: &Client,
        group: Pubkey,
        owner: &Keypair,
        payer: &Keypair, // pays the SOL for the new account
        mango_account_name: &str,
    ) -> anyhow::Result<Pubkey> {
        let program = client.anchor_client().program(mango_v4::ID);

        // Mango Account
        let mut mango_account_tuples = fetch_mango_accounts(&program, group, owner.pubkey())?;
        let mango_account_opt = mango_account_tuples
            .iter()
            .find(|(_, account)| account.fixed.name() == mango_account_name);
        if mango_account_opt.is_none() {
            mango_account_tuples.sort_by(|a, b| {
                a.1.fixed
                    .account_num
                    .partial_cmp(&b.1.fixed.account_num)
                    .unwrap()
            });
            let account_num = match mango_account_tuples.last() {
                Some(tuple) => tuple.1.fixed.account_num + 1,
                None => 0u32,
            };
            Self::create_account(client, group, owner, payer, account_num, mango_account_name)
                .context("Failed to create account...")?;
        }
        let mango_account_tuples = fetch_mango_accounts(&program, group, owner.pubkey())?;
        let index = mango_account_tuples
            .iter()
            .position(|tuple| tuple.1.fixed.name() == mango_account_name)
            .unwrap();
        Ok(mango_account_tuples[index].0)
    }

    pub fn create_account(
        client: &Client,
        group: Pubkey,
        owner: &Keypair,
        payer: &Keypair, // pays the SOL for the new account
        account_num: u32,
        mango_account_name: &str,
    ) -> anyhow::Result<(Pubkey, Signature)> {
        let program = client.anchor_client().program(mango_v4::ID);
        let account = Pubkey::find_program_address(
            &[
                group.as_ref(),
                b"MangoAccount".as_ref(),
                owner.pubkey().as_ref(),
                &account_num.to_le_bytes(),
            ],
            &mango_v4::id(),
        )
        .0;
        let txsig = program
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: anchor_lang::ToAccountMetas::to_account_metas(
                    &mango_v4::accounts::AccountCreate {
                        group,
                        owner: owner.pubkey(),
                        account,
                        payer: payer.pubkey(),
                        system_program: System::id(),
                    },
                    None,
                ),
                data: anchor_lang::InstructionData::data(&mango_v4::instruction::AccountCreate {
                    account_num,
                    name: mango_account_name.to_owned(),
                    token_count: 8,
                    serum3_count: 8,
                    perp_count: 8,
                    perp_oo_count: 8,
                }),
            })
            .signer(owner)
            .signer(payer)
            .send()
            .map_err(prettify_client_error)?;

        Ok((account, txsig))
    }

    /// Conveniently creates a RPC based client
    pub fn new_for_existing_account(
        client: Client,
        account: Pubkey,
        owner: Keypair,
    ) -> anyhow::Result<Self> {
        let rpc = client.rpc();
        let account_fetcher = Arc::new(CachedAccountFetcher::new(RpcAccountFetcher { rpc }));
        let mango_account = account_fetcher_fetch_mango_account(&*account_fetcher, account)?;
        let group = mango_account.fixed.group;
        if mango_account.fixed.owner != owner.pubkey() {
            anyhow::bail!(
                "bad owner for account: expected {} got {}",
                mango_account.fixed.owner,
                owner.pubkey()
            );
        }

        let group_context =
            MangoGroupContext::new_from_rpc(group, client.cluster.clone(), client.commitment)?;

        Self::new_detail(client, account, owner, group_context, account_fetcher)
    }

    /// Allows control of AccountFetcher and externally created MangoGroupContext
    pub fn new_detail(
        client: Client,
        account: Pubkey,
        owner: Keypair,
        // future: maybe pass Arc<MangoGroupContext>, so it can be extenally updated?
        group_context: MangoGroupContext,
        account_fetcher: Arc<dyn AccountFetcher>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client,
            account_fetcher,
            owner,
            mango_account_address: account,
            context: group_context,
            http_client: reqwest::Client::new(),
            runtime: Some(
                tokio::runtime::Builder::new_current_thread()
                    .thread_name("mango-client")
                    .enable_io()
                    .enable_time()
                    .build()
                    .unwrap(),
            ),
        })
    }

    pub fn anchor_client(&self) -> anchor_client::Client {
        self.client.anchor_client()
    }

    pub fn program(&self) -> Program {
        self.anchor_client().program(mango_v4::ID)
    }

    pub fn owner(&self) -> Pubkey {
        self.owner.pubkey()
    }

    pub fn group(&self) -> Pubkey {
        self.context.group
    }

    pub fn mango_account(&self) -> anyhow::Result<MangoAccountValue> {
        account_fetcher_fetch_mango_account(&*self.account_fetcher, self.mango_account_address)
    }

    pub fn first_bank(&self, token_index: TokenIndex) -> anyhow::Result<Bank> {
        let bank_address = self.context.mint_info(token_index).first_bank();
        account_fetcher_fetch_anchor_account(&*self.account_fetcher, bank_address)
    }

    pub fn derive_health_check_remaining_account_metas(
        &self,
        affected_tokens: Vec<TokenIndex>,
        writable_banks: bool,
    ) -> anyhow::Result<Vec<AccountMeta>> {
        let account = self.mango_account()?;
        self.context.derive_health_check_remaining_account_metas(
            &account,
            affected_tokens,
            writable_banks,
        )
    }

    pub fn derive_liquidation_health_check_remaining_account_metas(
        &self,
        liqee: &MangoAccountValue,
        asset_token_index: TokenIndex,
        liab_token_index: TokenIndex,
    ) -> anyhow::Result<Vec<AccountMeta>> {
        // figure out all the banks/oracles that need to be passed for the health check
        let mut banks = vec![];
        let mut oracles = vec![];
        let account = self.mango_account()?;

        let token_indexes = liqee
            .token_iter_active()
            .chain(account.token_iter_active())
            .map(|ta| ta.token_index)
            .unique();

        for token_index in token_indexes {
            let mint_info = self.context.mint_info(token_index);
            let writable_bank = token_index == asset_token_index || token_index == liab_token_index;
            banks.push((mint_info.first_bank(), writable_bank));
            oracles.push(mint_info.oracle);
        }

        let serum_oos = liqee
            .serum3_iter_active()
            .chain(account.serum3_iter_active())
            .map(|&s| s.open_orders);
        let perp_markets = liqee
            .perp_iter_active_accounts()
            .chain(account.perp_iter_active_accounts())
            .map(|&pa| self.context.perp_market_address(pa.market_index));

        Ok(banks
            .iter()
            .map(|(pubkey, is_writable)| AccountMeta {
                pubkey: *pubkey,
                is_writable: *is_writable,
                is_signer: false,
            })
            .chain(oracles.into_iter().map(to_readonly_account_meta))
            .chain(perp_markets.map(to_readonly_account_meta))
            .chain(serum_oos.map(to_readonly_account_meta))
            .collect())
    }

    pub fn token_deposit(&self, mint: Pubkey, amount: u64) -> anyhow::Result<Signature> {
        let token = self.context.token_by_mint(&mint)?;
        let token_index = token.token_index;
        let mint_info = token.mint_info;

        let health_check_metas =
            self.derive_health_check_remaining_account_metas(vec![token_index], false)?;

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: {
                    let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                        &mango_v4::accounts::TokenDeposit {
                            group: self.group(),
                            account: self.mango_account_address,
                            bank: mint_info.first_bank(),
                            vault: mint_info.first_vault(),
                            token_account: get_associated_token_address(
                                &self.owner(),
                                &mint_info.mint,
                            ),
                            token_authority: self.owner(),
                            token_program: Token::id(),
                        },
                        None,
                    );
                    ams.extend(health_check_metas.into_iter());
                    ams
                },
                data: anchor_lang::InstructionData::data(&mango_v4::instruction::TokenDeposit {
                    amount,
                }),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    pub fn get_oracle_price(
        &self,
        token_name: &str,
    ) -> Result<pyth_sdk_solana::Price, anyhow::Error> {
        let token_index = *self.context.token_indexes_by_name.get(token_name).unwrap();
        let mint_info = self.context.mint_info(token_index);
        let oracle_account = self.account_fetcher.fetch_raw_account(mint_info.oracle)?;
        Ok(pyth_sdk_solana::load_price(&oracle_account.data).unwrap())
    }

    //
    // Serum3
    //

    pub fn serum3_create_open_orders(&self, name: &str) -> anyhow::Result<Signature> {
        let account_pubkey = self.mango_account_address;

        let market_index = *self
            .context
            .serum3_market_indexes_by_name
            .get(name)
            .unwrap();
        let serum3_info = self.context.serum3_markets.get(&market_index).unwrap();

        let open_orders = Pubkey::find_program_address(
            &[
                account_pubkey.as_ref(),
                b"Serum3OO".as_ref(),
                serum3_info.address.as_ref(),
            ],
            &self.program().id(),
        )
        .0;

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: anchor_lang::ToAccountMetas::to_account_metas(
                    &mango_v4::accounts::Serum3CreateOpenOrders {
                        group: self.group(),
                        account: account_pubkey,

                        serum_market: serum3_info.address,
                        serum_program: serum3_info.market.serum_program,
                        serum_market_external: serum3_info.market.serum_market_external,
                        open_orders,
                        owner: self.owner(),
                        payer: self.owner(),
                        system_program: System::id(),
                        rent: sysvar::rent::id(),
                    },
                    None,
                ),
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::Serum3CreateOpenOrders {},
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    fn serum3_data<'a>(&'a self, name: &str) -> Result<Serum3Data<'a>, ClientError> {
        let market_index = *self
            .context
            .serum3_market_indexes_by_name
            .get(name)
            .unwrap();
        let serum3_info = self.context.serum3_markets.get(&market_index).unwrap();

        let quote_info = self.context.token(serum3_info.market.quote_token_index);
        let base_info = self.context.token(serum3_info.market.base_token_index);

        Ok(Serum3Data {
            market_index,
            market: serum3_info,
            quote: quote_info,
            base: base_info,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn serum3_place_order(
        &self,
        name: &str,
        side: Serum3Side,
        price: f64,
        size: f64,
        self_trade_behavior: Serum3SelfTradeBehavior,
        order_type: Serum3OrderType,
        client_order_id: u64,
        limit: u16,
    ) -> anyhow::Result<Signature> {
        let s3 = self.serum3_data(name)?;

        let account = self.mango_account()?;
        let open_orders = account.serum3_find(s3.market_index).unwrap().open_orders;

        let health_check_metas = self.derive_health_check_remaining_account_metas(vec![], false)?;

        // https://github.com/project-serum/serum-ts/blob/master/packages/serum/src/market.ts#L1306
        let limit_price = {
            (price * ((10u64.pow(s3.quote.decimals as u32) * s3.market.coin_lot_size) as f64))
                as u64
                / (10u64.pow(s3.base.decimals as u32) * s3.market.pc_lot_size)
        };
        // https://github.com/project-serum/serum-ts/blob/master/packages/serum/src/market.ts#L1333
        let max_base_qty =
            { (size * 10u64.pow(s3.base.decimals as u32) as f64) as u64 / s3.market.coin_lot_size };
        let max_native_quote_qty_including_fees = {
            fn get_fee_tier(msrm_balance: u64, srm_balance: u64) -> u64 {
                if msrm_balance >= 1 {
                    6
                } else if srm_balance >= 1_000_000 {
                    5
                } else if srm_balance >= 100_000 {
                    4
                } else if srm_balance >= 10_000 {
                    3
                } else if srm_balance >= 1_000 {
                    2
                } else if srm_balance >= 100 {
                    1
                } else {
                    0
                }
            }

            fn get_fee_rates(fee_tier: u64) -> (f64, f64) {
                if fee_tier == 1 {
                    // SRM2
                    return (0.002, -0.0003);
                } else if fee_tier == 2 {
                    // SRM3
                    return (0.0018, -0.0003);
                } else if fee_tier == 3 {
                    // SRM4
                    return (0.0016, -0.0003);
                } else if fee_tier == 4 {
                    // SRM5
                    return (0.0014, -0.0003);
                } else if fee_tier == 5 {
                    // SRM6
                    return (0.0012, -0.0003);
                } else if fee_tier == 6 {
                    // MSRM
                    return (0.001, -0.0005);
                }
                // Base
                (0.0022, -0.0003)
            }

            let fee_tier = get_fee_tier(0, 0);
            let rates = get_fee_rates(fee_tier);
            (s3.market.pc_lot_size as f64 * (1f64 + rates.0)) as u64 * (limit_price * max_base_qty)
        };

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: {
                    let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                        &mango_v4::accounts::Serum3PlaceOrder {
                            group: self.group(),
                            account: self.mango_account_address,
                            open_orders,
                            quote_bank: s3.quote.mint_info.first_bank(),
                            quote_vault: s3.quote.mint_info.first_vault(),
                            base_bank: s3.base.mint_info.first_bank(),
                            base_vault: s3.base.mint_info.first_vault(),
                            serum_market: s3.market.address,
                            serum_program: s3.market.market.serum_program,
                            serum_market_external: s3.market.market.serum_market_external,
                            market_bids: s3.market.bids,
                            market_asks: s3.market.asks,
                            market_event_queue: s3.market.event_q,
                            market_request_queue: s3.market.req_q,
                            market_base_vault: s3.market.coin_vault,
                            market_quote_vault: s3.market.pc_vault,
                            market_vault_signer: s3.market.vault_signer,
                            owner: self.owner(),
                            token_program: Token::id(),
                        },
                        None,
                    );
                    ams.extend(health_check_metas.into_iter());
                    ams
                },
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::Serum3PlaceOrder {
                        side,
                        limit_price,
                        max_base_qty,
                        max_native_quote_qty_including_fees,
                        self_trade_behavior,
                        order_type,
                        client_order_id,
                        limit,
                    },
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    pub fn serum3_settle_funds(&self, name: &str) -> anyhow::Result<Signature> {
        let s3 = self.serum3_data(name)?;

        let account = self.mango_account()?;
        let open_orders = account.serum3_find(s3.market_index).unwrap().open_orders;

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: anchor_lang::ToAccountMetas::to_account_metas(
                    &mango_v4::accounts::Serum3SettleFunds {
                        group: self.group(),
                        account: self.mango_account_address,
                        open_orders,
                        quote_bank: s3.quote.mint_info.first_bank(),
                        quote_vault: s3.quote.mint_info.first_vault(),
                        base_bank: s3.base.mint_info.first_bank(),
                        base_vault: s3.base.mint_info.first_vault(),
                        serum_market: s3.market.address,
                        serum_program: s3.market.market.serum_program,
                        serum_market_external: s3.market.market.serum_market_external,
                        market_base_vault: s3.market.coin_vault,
                        market_quote_vault: s3.market.pc_vault,
                        market_vault_signer: s3.market.vault_signer,
                        owner: self.owner(),
                        token_program: Token::id(),
                    },
                    None,
                ),
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::Serum3SettleFunds {},
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    pub fn serum3_cancel_all_orders(&self, market_name: &str) -> Result<Vec<u128>, anyhow::Error> {
        let market_index = *self
            .context
            .serum3_market_indexes_by_name
            .get(market_name)
            .unwrap();
        let account = self.mango_account()?;
        let open_orders = account.serum3_find(market_index).unwrap().open_orders;

        let open_orders_bytes = self.account_fetcher.fetch_raw_account(open_orders)?.data;
        let open_orders_data: &serum_dex::state::OpenOrders = bytemuck::from_bytes(
            &open_orders_bytes[5..5 + std::mem::size_of::<serum_dex::state::OpenOrders>()],
        );

        let mut orders = vec![];
        for order_id in open_orders_data.orders {
            if order_id != 0 {
                // TODO: find side for order_id, and only cancel the relevant order
                self.serum3_cancel_order(market_name, Serum3Side::Bid, order_id)
                    .ok();
                self.serum3_cancel_order(market_name, Serum3Side::Ask, order_id)
                    .ok();

                orders.push(order_id);
            }
        }

        Ok(orders)
    }

    pub fn serum3_cancel_order(
        &self,
        market_name: &str,
        side: Serum3Side,
        order_id: u128,
    ) -> anyhow::Result<()> {
        let s3 = self.serum3_data(market_name)?;

        let account = self.mango_account()?;
        let open_orders = account.serum3_find(s3.market_index).unwrap().open_orders;

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: {
                    anchor_lang::ToAccountMetas::to_account_metas(
                        &mango_v4::accounts::Serum3CancelOrder {
                            group: self.group(),
                            account: self.mango_account_address,
                            serum_market: s3.market.address,
                            serum_program: s3.market.market.serum_program,
                            serum_market_external: s3.market.market.serum_market_external,
                            open_orders,
                            market_bids: s3.market.bids,
                            market_asks: s3.market.asks,
                            market_event_queue: s3.market.event_q,
                            owner: self.owner(),
                        },
                        None,
                    )
                },
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::Serum3CancelOrder { side, order_id },
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)?;

        Ok(())
    }

    //
    // Perps
    //

    //
    // Liquidation
    //

    pub fn liq_token_with_token(
        &self,
        liqee: (&Pubkey, &MangoAccountValue),
        asset_token_index: TokenIndex,
        liab_token_index: TokenIndex,
        max_liab_transfer: I80F48,
    ) -> anyhow::Result<Signature> {
        let health_remaining_ams = self
            .derive_liquidation_health_check_remaining_account_metas(
                liqee.1,
                asset_token_index,
                liab_token_index,
            )
            .unwrap();

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: {
                    let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                        &mango_v4::accounts::LiqTokenWithToken {
                            group: self.group(),
                            liqee: *liqee.0,
                            liqor: self.mango_account_address,
                            liqor_owner: self.owner(),
                        },
                        None,
                    );
                    ams.extend(health_remaining_ams);
                    ams
                },
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::LiqTokenWithToken {
                        asset_token_index,
                        liab_token_index,
                        max_liab_transfer,
                    },
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    pub fn liq_token_bankruptcy(
        &self,
        liqee: (&Pubkey, &MangoAccountValue),
        liab_token_index: TokenIndex,
        max_liab_transfer: I80F48,
    ) -> anyhow::Result<Signature> {
        let quote_token_index = 0;

        let quote_info = self.context.token(quote_token_index);
        let liab_info = self.context.token(liab_token_index);

        let bank_remaining_ams = liab_info
            .mint_info
            .banks()
            .iter()
            .map(|bank_pubkey| to_writable_account_meta(*bank_pubkey))
            .collect::<Vec<_>>();

        let health_remaining_ams = self
            .derive_liquidation_health_check_remaining_account_metas(
                liqee.1,
                quote_token_index,
                liab_token_index,
            )
            .unwrap();

        let group = account_fetcher_fetch_anchor_account::<Group>(
            &*self.account_fetcher,
            self.context.group,
        )?;

        self.program()
            .request()
            .instruction(Instruction {
                program_id: mango_v4::id(),
                accounts: {
                    let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                        &mango_v4::accounts::LiqTokenBankruptcy {
                            group: self.group(),
                            liqee: *liqee.0,
                            liqor: self.mango_account_address,
                            liqor_owner: self.owner(),
                            liab_mint_info: liab_info.mint_info_address,
                            quote_vault: quote_info.mint_info.first_vault(),
                            insurance_vault: group.insurance_vault,
                            token_program: Token::id(),
                        },
                        None,
                    );
                    ams.extend(bank_remaining_ams);
                    ams.extend(health_remaining_ams);
                    ams
                },
                data: anchor_lang::InstructionData::data(
                    &mango_v4::instruction::LiqTokenBankruptcy {
                        liab_token_index,
                        max_liab_transfer,
                    },
                ),
            })
            .signer(&self.owner)
            .send()
            .map_err(prettify_client_error)
    }

    pub fn jupiter_swap(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        source_amount: u64,
        slippage: f64,
    ) -> anyhow::Result<Signature> {
        self.invoke(self.jupiter_swap_async(input_mint, output_mint, source_amount, slippage))
    }

    // Not actually fully async, since it uses the blocking RPC client to send the actual tx
    pub async fn jupiter_swap_async(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        source_amount: u64,
        slippage: f64,
    ) -> anyhow::Result<Signature> {
        let source_token = self.context.token_by_mint(&input_mint)?;
        let target_token = self.context.token_by_mint(&output_mint)?;

        let quote = self
            .http_client
            .get("https://quote-api.jup.ag/v1/quote")
            .query(&[
                ("inputMint", input_mint.to_string()),
                ("outputMint", output_mint.to_string()),
                ("amount", format!("{}", source_amount)),
                ("onlyDirectRoutes", "true".into()),
                ("filterTopNResult", "10".into()),
                ("slippage", format!("{}", slippage)),
            ])
            .send()
            .await
            .context("quote request to jupiter")?
            .json::<jupiter::QueryResult>()
            .await
            .context("receiving json response from jupiter quote request")?;

        // Find the top route that doesn't involve Raydium (that has too many accounts)
        let route = quote
            .data
            .iter()
            .find(|route| {
                !route
                    .market_infos
                    .iter()
                    .any(|mi| mi.label.contains("Raydium"))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no route for swap. found {} routes, but none were usable",
                    quote.data.len()
                )
            })?;

        let swap = self
            .http_client
            .post("https://quote-api.jup.ag/v1/swap")
            .json(&jupiter::SwapRequest {
                route: route.clone(),
                user_public_key: self.owner.pubkey().to_string(),
                wrap_unwrap_sol: false,
            })
            .send()
            .await
            .context("swap transaction request to jupiter")?
            .json::<jupiter::SwapResponse>()
            .await
            .context("receiving json response from jupiter swap transaction request")?;

        if swap.setup_transaction.is_some() || swap.cleanup_transaction.is_some() {
            anyhow::bail!(
                "chosen jupiter route requires setup or cleanup transactions, can't execute"
            );
        }

        // TODO: deal with versioned transaction!
        let jup_tx = bincode::options()
            .with_fixint_encoding()
            .reject_trailing_bytes()
            .deserialize::<solana_sdk::transaction::Transaction>(
                &base64::decode(&swap.swap_transaction)
                    .context("base64 decoding jupiter transaction")?,
            )
            .context("parsing jupiter transaction")?;
        let jup_ixs = deserialize_instructions(&jup_tx.message)
            .into_iter()
            // TODO: possibly creating associated token accounts if they don't exist yet is good?!
            // we could squeeze the FlashLoan instructions in the middle:
            //   - beginning AToken...
            //   - FlashLoanBegin
            //   - other JUP ix
            //   - FlashLoanEnd
            //   - ending AToken
            .filter(|ix| {
                ix.program_id
                    != Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
            })
            .collect::<Vec<_>>();

        let bank_ams = [
            source_token.mint_info.first_bank(),
            target_token.mint_info.first_bank(),
        ]
        .into_iter()
        .map(to_writable_account_meta)
        .collect::<Vec<_>>();

        let vault_ams = [
            source_token.mint_info.first_vault(),
            target_token.mint_info.first_vault(),
        ]
        .into_iter()
        .map(to_writable_account_meta)
        .collect::<Vec<_>>();

        let token_ams = [source_token.mint_info.mint, target_token.mint_info.mint]
            .into_iter()
            .map(|mint| {
                to_writable_account_meta(
                    anchor_spl::associated_token::get_associated_token_address(
                        &self.owner(),
                        &mint,
                    ),
                )
            })
            .collect::<Vec<_>>();

        let loan_amounts = vec![source_amount, 0u64];

        // This relies on the fact that health account banks will be identical to the first_bank above!
        let health_ams = self
            .derive_health_check_remaining_account_metas(
                vec![source_token.token_index, target_token.token_index],
                true,
            )
            .context("building health accounts")?;

        let program = self.program();
        let mut builder = program.request().instruction(Instruction {
            program_id: mango_v4::id(),
            accounts: {
                let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                    &mango_v4::accounts::FlashLoanBegin {
                        group: self.group(),
                        token_program: Token::id(),
                        instructions: solana_sdk::sysvar::instructions::id(),
                    },
                    None,
                );
                ams.extend(bank_ams);
                ams.extend(vault_ams.clone());
                ams.extend(token_ams.clone());
                ams
            },
            data: anchor_lang::InstructionData::data(&mango_v4::instruction::FlashLoanBegin {
                loan_amounts,
            }),
        });
        for ix in jup_ixs {
            builder = builder.instruction(ix);
        }
        builder = builder.instruction(Instruction {
            program_id: mango_v4::id(),
            accounts: {
                let mut ams = anchor_lang::ToAccountMetas::to_account_metas(
                    &mango_v4::accounts::FlashLoanEnd {
                        account: self.mango_account_address,
                        owner: self.owner(),
                        token_program: Token::id(),
                    },
                    None,
                );
                ams.extend(health_ams);
                ams.extend(vault_ams);
                ams.extend(token_ams);
                ams
            },
            data: anchor_lang::InstructionData::data(&mango_v4::instruction::FlashLoanEnd {}),
        });

        let rpc = self.client.rpc_async();
        builder
            .signer(&self.owner)
            .send_rpc_async(&rpc)
            .await
            .map_err(prettify_client_error)
    }

    fn invoke<T, F: std::future::Future<Output = T>>(&self, f: F) -> T {
        // `block_on()` panics if called within an asynchronous execution context. Whereas
        // `block_in_place()` only panics if called from a current_thread runtime, which is the
        // lesser evil.
        tokio::task::block_in_place(move || self.runtime.as_ref().expect("runtime").block_on(f))
    }
}

fn deserialize_instructions(message: &solana_sdk::message::Message) -> Vec<Instruction> {
    message
        .instructions
        .iter()
        .map(|ci| solana_sdk::instruction::Instruction {
            program_id: *ci.program_id(&message.account_keys),
            accounts: ci
                .accounts
                .iter()
                .map(|&index| AccountMeta {
                    pubkey: message.account_keys[index as usize],
                    is_signer: message.is_signer(index.into()),
                    is_writable: message.is_writable(index.into()),
                })
                .collect(),
            data: ci.data.clone(),
        })
        .collect()
}

struct Serum3Data<'a> {
    market_index: Serum3MarketIndex,
    market: &'a Serum3MarketContext,
    quote: &'a TokenContext,
    base: &'a TokenContext,
}

#[derive(Debug, thiserror::Error)]
pub enum MangoClientError {
    #[error("Transaction simulation error. Logs: {logs}")]
    SendTransactionPreflightFailure { logs: String },
}

/// Do some manual unpacking on some ClientErrors
///
/// Unfortunately solana's RpcResponseError will very unhelpfully print [N log messages]
/// instead of showing the actual log messages. This unpacks the error to provide more useful
/// output.
pub fn prettify_client_error(err: anchor_client::ClientError) -> anyhow::Error {
    use solana_client::client_error::ClientErrorKind;
    use solana_client::rpc_request::{RpcError, RpcResponseErrorData};
    match &err {
        anchor_client::ClientError::SolanaClientError(c) => {
            match c.kind() {
                ClientErrorKind::RpcError(RpcError::RpcResponseError { data, .. }) => match data {
                    RpcResponseErrorData::SendTransactionPreflightFailure(s) => {
                        if let Some(logs) = s.logs.as_ref() {
                            return MangoClientError::SendTransactionPreflightFailure {
                                logs: logs.iter().join("; "),
                            }
                            .into();
                        }
                    }
                    _ => {}
                },
                _ => {}
            };
        }
        _ => {}
    };
    err.into()
}

pub fn keypair_from_cli(keypair: &str) -> Keypair {
    let maybe_keypair = keypair::read_keypair(&mut keypair.as_bytes());
    match maybe_keypair {
        Ok(keypair) => keypair,
        Err(_) => {
            let path = std::path::PathBuf::from_str(&*shellexpand::tilde(keypair)).unwrap();
            keypair::read_keypair_file(path)
                .unwrap_or_else(|_| panic!("Failed to read keypair from {}", keypair))
        }
    }
}

pub fn pubkey_from_cli(pubkey: &str) -> Pubkey {
    match Pubkey::from_str(pubkey) {
        Ok(p) => p,
        Err(_) => keypair_from_cli(pubkey).pubkey(),
    }
}

fn to_readonly_account_meta(pubkey: Pubkey) -> AccountMeta {
    AccountMeta {
        pubkey,
        is_writable: false,
        is_signer: false,
    }
}

fn to_writable_account_meta(pubkey: Pubkey) -> AccountMeta {
    AccountMeta {
        pubkey,
        is_writable: true,
        is_signer: false,
    }
}
