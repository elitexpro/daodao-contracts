use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg, Threshold, Vote};
use crate::query::{
    AllBalancesResponse, ConfigResponse, ProposalListResponse, ProposalResponse, Status,
    ThresholdResponse, TokenListResponse, VoteInfo, VoteListResponse, VoteResponse, VoterResponse,
};
use crate::state::{
    next_id, parse_id, Ballot, Config, Proposal, ProposalDeposit, Votes, BALLOTS, CONFIG,
    PROPOSALS, TREASURY_TOKENS,
};
use cosmwasm_std::{
    entry_point, to_binary, Addr, Binary, BlockInfo, CosmosMsg, Deps, DepsMut, Empty, Env,
    MessageInfo, Order, Response, StdResult, Uint128, WasmMsg,
};
use cw0::{maybe_addr, Duration, Expiration};
use cw2::set_contract_version;
use cw20::{
    BalanceResponse, Cw20CoinVerified, Cw20Contract, Cw20ExecuteMsg, Cw20QueryMsg, Cw20ReceiveMsg,
};
use cw20_gov::msg::{BalanceAtHeightResponse, QueryMsg as Cw20GovQueryMsg};
use cw20_gov::state::TokenInfo;
use cw_storage_plus::Bound;
use std::cmp::Ordering;

// version info for migration info
pub const CONTRACT_NAME: &str = "crates.io:sg_dao";
pub const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

// settings for pagination
const MAX_LIMIT: u32 = 30;
const DEFAULT_LIMIT: u32 = 10;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    let cw20_addr = Cw20Contract(deps.api.addr_validate(&msg.cw20_addr).map_err(|_| {
        ContractError::InvalidCw20 {
            addr: msg.cw20_addr.clone(),
        }
    })?);

    let proposal_deposit_cw20_addr = Cw20Contract(
        deps.api
            .addr_validate(&msg.proposal_deposit_token_address)
            .map_err(|_| ContractError::InvalidCw20 {
                addr: msg.proposal_deposit_token_address.clone(),
            })?,
    );

    // Add cw20-gov token to list of TREASURY TOKENS
    TREASURY_TOKENS.save(deps.storage, &vec![cw20_addr.addr()])?;

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    msg.threshold.validate()?;

    let cfg = Config {
        threshold: msg.threshold,
        max_voting_period: msg.max_voting_period,
        cw20_addr,
        proposal_deposit: ProposalDeposit {
            amount: msg.proposal_deposit_amount,
            token_address: proposal_deposit_cw20_addr,
        },
    };
    CONFIG.save(deps.storage, &cfg)?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<Empty>, ContractError> {
    match msg {
        ExecuteMsg::Propose {
            title,
            description,
            msgs,
            latest,
        } => execute_propose(deps, env, info, title, description, msgs, latest),
        ExecuteMsg::Vote { proposal_id, vote } => execute_vote(deps, env, info, proposal_id, vote),
        ExecuteMsg::Execute { proposal_id } => execute_execute(deps, env, info, proposal_id),
        ExecuteMsg::Close { proposal_id } => execute_close(deps, env, info, proposal_id),
        ExecuteMsg::Receive(msg) => execute_receive(deps, info, msg),
        ExecuteMsg::UpdateConfig {
            threshold,
            max_voting_period,
            proposal_deposit_amount,
            proposal_deposit_token_address,
        } => execute_update_config(
            deps,
            env,
            info,
            threshold,
            max_voting_period,
            proposal_deposit_amount,
            proposal_deposit_token_address,
        ),
    }
}

pub fn execute_receive(
    deps: DepsMut,
    info: MessageInfo,
    _wrapper: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    // Save token address to Tresury token list for receiving balances
    let mut token_list = TREASURY_TOKENS.load(deps.storage)?;

    // Check that token isn't already added
    if token_list
        .clone()
        .into_iter()
        .find(|addr| addr == &info.sender)
        .is_none()
    {
        token_list.push(info.sender);
        TREASURY_TOKENS.save(deps.storage, &token_list)?;
    }

    Ok(Response::default())
}

pub fn execute_propose(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    title: String,
    description: String,
    msgs: Vec<CosmosMsg>,
    // we ignore earliest
    latest: Option<Expiration>,
) -> Result<Response<Empty>, ContractError> {
    let cfg = CONFIG.load(deps.storage)?;

    // Only owners of the social token can create a proposal
    let balance = get_balance(deps.as_ref(), info.sender.clone())?;
    if balance == Uint128::zero() {
        return Err(ContractError::Unauthorized {});
    }

    // max expires also used as default
    let max_expires = cfg.max_voting_period.after(&env.block);
    let mut expires = latest.unwrap_or(max_expires);
    let comp = expires.partial_cmp(&max_expires);
    if let Some(Ordering::Greater) = comp {
        expires = max_expires;
    } else if comp.is_none() {
        return Err(ContractError::WrongExpiration {});
    }

    // Get total supply
    let total_supply = get_total_supply(deps.as_ref())?;

    // create a proposal
    let mut prop = Proposal {
        title,
        description,
        proposer: info.sender.clone(),
        start_height: env.block.height,
        expires,
        msgs,
        status: Status::Open,
        // votes: Votes::new(vote_power),
        votes: Votes {
            yes: Uint128::zero(),
            no: Uint128::zero(),
            abstain: Uint128::zero(),
            veto: Uint128::zero(),
        },
        threshold: cfg.threshold.clone(),
        total_weight: total_supply,
        deposit: cfg.proposal_deposit.clone(),
    };
    prop.update_status(&env.block);
    let id = next_id(deps.storage)?;
    PROPOSALS.save(deps.storage, id.into(), &prop)?;

    let deposit_msg = get_deposit_message(&env, &info, &cfg.proposal_deposit)?;

    Ok(Response::new()
        .add_messages(deposit_msg)
        .add_attribute("action", "propose")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", id.to_string())
        .add_attribute("status", format!("{:?}", prop.status)))
}

fn get_deposit_message(
    env: &Env,
    info: &MessageInfo,
    config: &ProposalDeposit,
) -> StdResult<Vec<CosmosMsg>> {
    if config.amount == Uint128::zero() {
        return Ok(vec![]);
    }
    let transfer_cw20_msg = Cw20ExecuteMsg::TransferFrom {
        owner: info.sender.clone().into(),
        recipient: env.contract.address.clone().into(),
        amount: config.amount,
    };
    let exec_cw20_transfer = WasmMsg::Execute {
        contract_addr: config.token_address.addr().into(),
        msg: to_binary(&transfer_cw20_msg)?,
        funds: vec![],
    };
    let cw20_transfer_cosmos_msg: CosmosMsg = exec_cw20_transfer.into();
    Ok(vec![cw20_transfer_cosmos_msg])
}

pub fn execute_vote(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
    vote: Vote,
) -> Result<Response<Empty>, ContractError> {
    // ensure proposal exists and can be voted on
    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    if prop.status != Status::Open {
        return Err(ContractError::NotOpen {});
    }
    if prop.expires.is_expired(&env.block) {
        return Err(ContractError::Expired {});
    }

    // Get voter balance at proposal start
    let vote_power = get_balance_at_height(deps.as_ref(), info.sender.clone(), prop.start_height)?;

    if vote_power == Uint128::zero() {
        return Err(ContractError::Unauthorized {});
    }

    // cast vote if no vote previously cast
    BALLOTS.update(
        deps.storage,
        (proposal_id.into(), &info.sender),
        |bal| match bal {
            Some(_) => Err(ContractError::AlreadyVoted {}),
            None => Ok(Ballot {
                weight: vote_power,
                vote,
            }),
        },
    )?;

    // update vote tally
    prop.votes.add_vote(vote, vote_power);
    prop.update_status(&env.block);
    PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;

    Ok(Response::new()
        .add_attribute("action", "vote")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string())
        .add_attribute("status", format!("{:?}", prop.status)))
}

pub fn execute_execute(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    proposal_id: u64,
) -> Result<Response, ContractError> {
    // anyone can trigger this if the vote passed

    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    // we allow execution even after the proposal "expiration" as long as all vote come in before
    // that point. If it was approved on time, it can be executed any time.
    if prop.status != Status::Passed {
        return Err(ContractError::WrongExecuteStatus {});
    }

    // set it to executed
    prop.status = Status::Executed;
    PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;

    let refund_msg = get_proposal_deposit_refund_message(&prop.proposer, &prop.deposit)?;

    // dispatch all proposed messages
    Ok(Response::new()
        .add_messages(refund_msg)
        .add_messages(prop.msgs)
        .add_attribute("action", "execute")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string()))
}

fn get_proposal_deposit_refund_message(
    proposer: &Addr,
    config: &ProposalDeposit,
) -> StdResult<Vec<CosmosMsg>> {
    if config.amount == Uint128::zero() {
        return Ok(vec![]);
    }
    let transfer_cw20_msg = Cw20ExecuteMsg::Transfer {
        recipient: proposer.into(),
        amount: config.amount,
    };
    let exec_cw20_transfer = WasmMsg::Execute {
        contract_addr: config.token_address.addr().into(),
        msg: to_binary(&transfer_cw20_msg)?,
        funds: vec![],
    };
    let cw20_transfer_cosmos_msg: CosmosMsg = exec_cw20_transfer.into();
    Ok(vec![cw20_transfer_cosmos_msg])
}

pub fn execute_close(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
) -> Result<Response<Empty>, ContractError> {
    // anyone can trigger this if the vote passed
    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    if [Status::Executed, Status::Rejected, Status::Passed]
        .iter()
        .any(|x| *x == prop.status)
    {
        return Err(ContractError::WrongCloseStatus {});
    }
    if !prop.expires.is_expired(&env.block) {
        return Err(ContractError::NotExpired {});
    }

    // set it to failed
    prop.status = Status::Rejected;
    PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;

    Ok(Response::new()
        .add_attribute("action", "close")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string()))
}

pub fn execute_update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    threshold: Threshold,
    max_voting_period: Duration,
    proposal_deposit_amount: Uint128,
    proposal_deposit_token_address: String,
) -> Result<Response<Empty>, ContractError> {
    // Only contract can call this method
    if env.contract.address != info.sender {
        return Err(ContractError::Unauthorized {});
    }

    threshold.validate()?;

    let proposal_deposit_cw20_addr = Cw20Contract(
        deps.api
            .addr_validate(&proposal_deposit_token_address)
            .map_err(|_| ContractError::InvalidCw20 {
                addr: proposal_deposit_token_address.clone(),
            })?,
    );

    CONFIG.update(deps.storage, |mut exists| -> StdResult<_> {
        exists.threshold = threshold;
        exists.max_voting_period = max_voting_period;
        exists.proposal_deposit = ProposalDeposit {
            amount: proposal_deposit_amount,
            token_address: proposal_deposit_cw20_addr,
        };
        Ok(exists)
    })?;

    Ok(Response::new()
        .add_attribute("action", "update_config")
        .add_attribute("sender", info.sender))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Threshold {} => to_binary(&query_threshold(deps)?),
        QueryMsg::Proposal { proposal_id } => to_binary(&query_proposal(deps, env, proposal_id)?),
        QueryMsg::Vote { proposal_id, voter } => to_binary(&query_vote(deps, proposal_id, voter)?),
        QueryMsg::ListProposals { start_after, limit } => {
            to_binary(&list_proposals(deps, env, start_after, limit)?)
        }
        QueryMsg::ReverseProposals {
            start_before,
            limit,
        } => to_binary(&reverse_proposals(deps, env, start_before, limit)?),
        QueryMsg::ListVotes {
            proposal_id,
            start_after,
            limit,
        } => to_binary(&list_votes(deps, proposal_id, start_after, limit)?),
        QueryMsg::Voter { address } => to_binary(&query_voter(deps, address)?),
        QueryMsg::AllBalances {} => to_binary(&query_all_balances(deps, env)?),
        QueryMsg::GetConfig {} => to_binary(&query_config(deps)?),
        QueryMsg::Cw20TokenList {} => to_binary(&query_cw20_token_list(deps)?),
    }
}

fn get_total_supply(deps: Deps) -> StdResult<Uint128> {
    let cfg = CONFIG.load(deps.storage)?;

    // Get total supply
    let token_info: TokenInfo = deps
        .querier
        .query_wasm_smart(cfg.cw20_addr.addr(), &Cw20QueryMsg::TokenInfo {})?;
    Ok(token_info.total_supply)
}

fn get_balance(deps: Deps, address: Addr) -> StdResult<Uint128> {
    let cfg = CONFIG.load(deps.storage)?;
    // Get total supply
    let balance: BalanceResponse = deps
        .querier
        .query_wasm_smart(
            cfg.cw20_addr.addr(),
            &Cw20QueryMsg::Balance {
                address: address.to_string(),
            },
        )
        .unwrap_or(BalanceResponse {
            balance: Uint128::zero(),
        });
    Ok(balance.balance)
}

fn get_balance_at_height(deps: Deps, address: Addr, height: u64) -> StdResult<Uint128> {
    let cfg = CONFIG.load(deps.storage)?;
    // Get total supply
    let balance: BalanceAtHeightResponse = deps
        .querier
        .query_wasm_smart(
            cfg.cw20_addr.addr(),
            &Cw20GovQueryMsg::BalanceAtHeight {
                address: address.to_string(),
                height,
            },
        )
        .unwrap_or(BalanceAtHeightResponse {
            balance: Uint128::zero(),
            height: 0,
        });
    Ok(balance.balance)
}

fn query_threshold(deps: Deps) -> StdResult<ThresholdResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    let total_supply = get_total_supply(deps)?;
    Ok(cfg.threshold.to_response(total_supply))
}

fn query_proposal(deps: Deps, env: Env, id: u64) -> StdResult<ProposalResponse> {
    let prop = PROPOSALS.load(deps.storage, id.into())?;
    let status = prop.current_status(&env.block);
    let total_supply = get_total_supply(deps)?;
    let threshold = prop.threshold.to_response(total_supply);
    Ok(ProposalResponse {
        id,
        title: prop.title,
        description: prop.description,
        proposer: prop.proposer,
        msgs: prop.msgs,
        status,
        expires: prop.expires,
        threshold,
        deposit_amount: prop.deposit.amount,
        deposit_token_address: prop.deposit.token_address.addr(),
    })
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse { config })
}

fn query_cw20_token_list(deps: Deps) -> StdResult<TokenListResponse> {
    let token_list = TREASURY_TOKENS.load(deps.storage)?;
    Ok(TokenListResponse { token_list })
}

fn query_all_balances(deps: Deps, env: Env) -> StdResult<AllBalancesResponse> {
    let native_balances = deps
        .querier
        .query_all_balances(env.contract.address.clone())?;

    let cw20_balances: Vec<Cw20CoinVerified> = TREASURY_TOKENS
        .load(deps.storage)?
        .into_iter()
        .map(|cw20_contract_address| {
            let balance: BalanceResponse = deps
                .querier
                .query_wasm_smart(
                    &cw20_contract_address,
                    &Cw20QueryMsg::Balance {
                        address: env.contract.address.to_string(),
                    },
                )
                .unwrap();

            return Cw20CoinVerified {
                address: cw20_contract_address,
                amount: balance.balance,
            };
        })
        .collect::<Vec<Cw20CoinVerified>>();

    Ok(AllBalancesResponse {
        native: native_balances,
        cw20: cw20_balances,
    })
}

fn list_proposals(
    deps: Deps,
    env: Env,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<ProposalListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after.map(Bound::exclusive_int);
    let props: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|p| map_proposal(&env.block, p))
        .collect();

    Ok(ProposalListResponse { proposals: props? })
}

fn reverse_proposals(
    deps: Deps,
    env: Env,
    start_before: Option<u64>,
    limit: Option<u32>,
) -> StdResult<ProposalListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let end = start_before.map(Bound::exclusive_int);
    let props: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, None, end, Order::Descending)
        .take(limit)
        .map(|p| map_proposal(&env.block, p))
        .collect();

    Ok(ProposalListResponse { proposals: props? })
}

fn map_proposal(
    block: &BlockInfo,
    item: StdResult<(Vec<u8>, Proposal)>,
) -> StdResult<ProposalResponse> {
    let (key, prop) = item?;
    let status = prop.current_status(block);
    let threshold = prop.threshold.to_response(prop.total_weight);
    Ok(ProposalResponse {
        id: parse_id(&key)?,
        title: prop.title,
        description: prop.description,
        proposer: prop.proposer,
        msgs: prop.msgs,
        status,
        expires: prop.expires,
        threshold,
        deposit_amount: prop.deposit.amount,
        deposit_token_address: prop.deposit.token_address.addr(),
    })
}

fn query_vote(deps: Deps, proposal_id: u64, voter: String) -> StdResult<VoteResponse> {
    let voter_addr = deps.api.addr_validate(&voter)?;
    let prop = BALLOTS.may_load(deps.storage, (proposal_id.into(), &voter_addr))?;
    let vote = prop.map(|b| VoteInfo {
        voter,
        vote: b.vote,
        weight: b.weight,
    });
    Ok(VoteResponse { vote })
}

fn list_votes(
    deps: Deps,
    proposal_id: u64,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<VoteListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let addr = maybe_addr(deps.api, start_after)?;
    let start = addr.map(|addr| Bound::exclusive(addr.as_ref()));

    let votes: StdResult<Vec<_>> = BALLOTS
        .prefix(proposal_id.into())
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (voter, ballot) = item?;
            Ok(VoteInfo {
                voter: String::from_utf8(voter)?,
                vote: ballot.vote,
                weight: ballot.weight,
            })
        })
        .collect();

    Ok(VoteListResponse { votes: votes? })
}

fn query_voter(deps: Deps, voter: String) -> StdResult<VoterResponse> {
    let voter_addr = deps.api.addr_validate(&voter)?;
    let weight = get_balance(deps, voter_addr)?;

    Ok(VoterResponse {
        weight: Some(weight),
    })
}
