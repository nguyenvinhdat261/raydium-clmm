use std::collections::VecDeque;
use std::ops::Deref;

use crate::error::ErrorCode;
use crate::libraries::tick_math;
use crate::swap::swap_internal;
use crate::util::*;
use crate::{states::*, util};
use anchor_lang::prelude::*;
use anchor_spl::token::Token;
use anchor_spl::token_interface::{Mint, Token2022, TokenAccount};

/// Memo msg for swap
pub const SWAP_MEMO_MSG: &'static [u8] = b"raydium_swap";
#[derive(Accounts)]
pub struct SwapSingleV2<'info> {
    /// The user performing the swap
    pub payer: Signer<'info>,

    /// The factory state to read protocol fees
    #[account(address = pool_state.load()?.amm_config)]
    pub amm_config: Box<Account<'info, AmmConfig>>,

    /// The program account of the pool in which the swap will be performed
    #[account(mut)]
    pub pool_state: AccountLoader<'info, PoolState>,

    /// The user token account for input token
    #[account(mut)]
    pub input_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The user token account for output token
    #[account(mut)]
    pub output_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for input token
    #[account(mut)]
    pub input_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for output token
    #[account(mut)]
    pub output_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The program account for the most recent oracle observation
    #[account(mut, address = pool_state.load()?.observation_key)]
    pub observation_state: AccountLoader<'info, ObservationState>,

    /// SPL program for token transfers
    pub token_program: Program<'info, Token>,

    /// SPL program 2022 for token transfers
    pub token_program_2022: Program<'info, Token2022>,

    /// CHECK:
    #[account(
        address = spl_memo::id()
    )]
    pub memo_program: UncheckedAccount<'info>,

    /// The mint of token vault 0
    #[account(
        address = input_vault.mint
    )]
    pub input_vault_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The mint of token vault 1
    #[account(
        address = output_vault.mint
    )]
    pub output_vault_mint: Box<InterfaceAccount<'info, Mint>>,
    // remaining accounts
    // tickarray_bitmap_extension: must add account if need regardless the sequence
    // tick_array_account_1
    // tick_array_account_2
    // tick_array_account_...
}

/// Performs a single exact input/output swap
/// if is_base_input = true, return vaule is the max_amount_out, otherwise is min_amount_in
pub fn exact_internal_v2<'c: 'info, 'info>(
    ctx: &mut SwapSingleV2<'info>,
    remaining_accounts: &'c [AccountInfo<'info>],
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<u64> {
    // Lấy thời gian hiện tại
    let block_timestamp = solana_program::clock::Clock::get()?.unix_timestamp as u64;

    // Xác định thứ tự chuyển đổi token và tính toán số lượng chuyển
    let amount_specified = if is_base_input {
        let transfer_fee =
            util::get_transfer_fee(ctx.input_vault_mint.clone(), amount_specified).unwrap();
        amount_specified - transfer_fee
    } else {
        let transfer_fee =
            util::get_transfer_inverse_fee(ctx.output_vault_mint.clone(), amount_specified)
                .unwrap();
        amount_specified + transfer_fee
    };

    // Kiểm tra điều kiện hợp lệ của pool và thời gian
    require_gt!(block_timestamp, ctx.pool_state.load()?.open_time);

    let zero_for_one = ctx.input_vault.mint == ctx.pool_state.load()?.token_mint_0;

    // Xác định các tài khoản đầu vào và đầu ra
    let (input_account, output_account, input_vault, output_vault, input_mint, output_mint) =
        if zero_for_one {
            (
                ctx.input_token_account.clone(),
                ctx.output_token_account.clone(),
                ctx.input_vault.clone(),
                ctx.output_vault.clone(),
                ctx.input_vault_mint.clone(),
                ctx.output_vault_mint.clone(),
            )
        } else {
            (
                ctx.output_token_account.clone(),
                ctx.input_token_account.clone(),
                ctx.output_vault.clone(),
                ctx.input_vault.clone(),
                ctx.output_vault_mint.clone(),
                ctx.input_vault_mint.clone(),
            )
        };

    // Tính toán phí chuyển đổi
    let transfer_fee_input = util::get_transfer_fee(input_mint.clone(), amount_specified).unwrap();
    let transfer_fee_output = util::get_transfer_inverse_fee(output_mint.clone(), amount_specified).unwrap();

    let amount_without_fee = if zero_for_one {
        amount_specified - transfer_fee_output
    } else {
        amount_specified - transfer_fee_input
    };

    // Chuyển token đầu vào từ người dùng đến pool
    transfer_from_user_to_pool_vault(
        &ctx.payer,
        &input_account,
        &input_vault,
        Some(input_mint),
        &ctx.token_program,
        Some(ctx.token_program_2022.to_account_info()),
        amount_specified,
    )?;

    // Chuyển token đầu ra từ pool đến người dùng
    transfer_from_pool_vault_to_user(
        &ctx.pool_state,
        &output_vault,
        &output_account,
        Some(output_mint),
        &ctx.token_program,
        Some(ctx.token_program_2022.to_account_info()),
        amount_without_fee,
    )?;

    // Reload lại tài khoản để cập nhật số dư
    ctx.output_token_account.reload()?;
    ctx.input_token_account.reload()?;

    // Phát sự kiện swap
    emit!(SwapEvent {
        pool_state: ctx.pool_state.key(),
        sender: ctx.payer.key(),
        token_account_0: input_account.key(),
        token_account_1: output_account.key(),
        amount_0: if zero_for_one { amount_specified } else { amount_without_fee },
        transfer_fee_0: transfer_fee_input,
        amount_1: if zero_for_one { amount_without_fee } else { amount_specified },
        transfer_fee_1: transfer_fee_output,
        zero_for_one,
        sqrt_price_x64: ctx.pool_state.load()?.sqrt_price_x64,
        liquidity: ctx.pool_state.load()?.liquidity,
        tick: ctx.pool_state.load()?.tick_current,
    });

    // Trả về số lượng token đầu ra đã swap
    if is_base_input {
        Ok(ctx.output_token_account.amount)
    } else {
        Ok(ctx.input_token_account.amount)
    }
}


pub fn swap_v2<'a, 'b, 'c: 'info, 'info>(
    ctx: Context<'a, 'b, 'c, 'info, SwapSingleV2<'info>>,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
) -> Result<()> {
    let amount_result = exact_internal_v2(
        ctx.accounts,
        ctx.remaining_accounts,
        amount,
        sqrt_price_limit_x64,
        is_base_input,
    )?;
    if is_base_input {
        require_gte!(
            amount_result,
            other_amount_threshold,
            ErrorCode::TooLittleOutputReceived
        );
    } else {
        require_gte!(
            other_amount_threshold,
            amount_result,
            ErrorCode::TooMuchInputPaid
        );
    }

    Ok(())
}
