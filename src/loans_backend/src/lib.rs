use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use serde::{Deserialize, Serialize};
use candid::{Principal, CandidType, candid_method};
use ic_cdk_macros::{init, query, update};
use ic_cdk::call;

#[derive(CandidType, Clone, Debug, Serialize, Deserialize)]
enum LoanStatus {
    PendingConfirmation,
    Confirmed,
    Rejected,
    Dropped,
}

#[derive(CandidType, Clone, Debug, Serialize, Deserialize)]
struct ActiveLoan {
    lender: Principal,
    borrower: Principal,
    btc_amount: u64,
    collateral_amount: u64,
    status: LoanStatus,
    signed_tx: Vec<u8>,
}

#[derive(CandidType, Clone, Debug, Serialize, Deserialize)]
struct LoanOffer {
    offer_id: u64,
    btc_amount: u64,
    runes_required: u64,
    max_ltv: f64,
}

#[derive(CandidType, Clone, Debug, Serialize, Deserialize)]
struct LenderInfo {
    btc_address: String,
    balance: u64,
    pending_txs: Vec<PendingTransaction>,
    loan_offers: Vec<LoanOffer>,
}

#[derive(CandidType, Clone, Debug, Serialize, Deserialize)]
struct PendingTransaction {
    amount: u64,
    confirmations: u8,
}

#[derive(Default)]
struct State {
    lenders: HashMap<Principal, LenderInfo>,
    next_offer_id: u64,
    active_loans: HashMap<u64, ActiveLoan>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut State) -> R,
{
    STATE.with(|state_cell| {
        let mut state = state_cell.borrow_mut();
        f(&mut state)
    })
}

#[init]
fn init() {
    start_cron_job();
    start_loan_monitoring_job();
}

#[update(name = "registerLender")]
#[candid_method(update, rename = "registerLender")]
async fn register_lender() -> String {
    let caller = ic_cdk::caller();
    let btc_address = generate_btc_address().await;

    let lender_info = LenderInfo {
        btc_address: btc_address.clone(),
        balance: 0,
        pending_txs: Vec::new(),
        loan_offers: Vec::new(),
    };

    with_state(|state| {
        state.lenders.insert(caller, lender_info);
    });

    btc_address
}

#[update(name = "updateLenderBalance")]
#[candid_method(update, rename = "updateLenderBalance")]
fn update_lender_balance(amount: u64) -> Result<(), String> {
    let caller = ic_cdk::caller();

    with_state(|state| {
        if let Some(lender_info) = state.lenders.get_mut(&caller) {
            lender_info.balance = amount;
            Ok(())
        } else {
            Err("Lender not registered".to_string())
        }
    })
}

#[update(name = "createLoanOffer")]
#[candid_method(update, rename = "createLoanOffer")]
fn create_loan_offer(
    btc_amount: u64,
    runes_required: u64,
    max_ltv: f64,
) -> Result<u64, String> {

    let caller = ic_cdk::caller();

    if btc_amount == 0 {
        return Err("BTC amount must be greater than zero".to_string());
    }
    if runes_required == 0 {
        return Err("Runes required must be greater than zero".to_string());
    }
    if max_ltv <= 0.0 || max_ltv > 1.0 {
        return Err("Max LTV must be between 0 and 1".to_string());
    }

    with_state(|state| {
        let lender_info = state.lenders.get_mut(&caller);
        match lender_info {
            Some(info) => {
                let offer_id = state.next_offer_id;
                state.next_offer_id += 1;

                let loan_offer = LoanOffer {
                    offer_id,
                    btc_amount,
                    runes_required,
                    max_ltv,
                };

                info.loan_offers.push(loan_offer);

                Ok(offer_id)
            }
            None => Err("Lender not registered".to_string()),
        }
    })
}

#[update(name = "acceptLoanOffer")]
#[candid_method(update, rename = "acceptLoanOffer")]
fn accept_loan_offer(
    offer_id: u64,
    unsigned_tx: Vec<u8>,
    runes: u64,
) -> Result<(), String> {
    let borrower = ic_cdk::caller();

    // Finding the loan offer and lender
    let (lender_principal, loan_offer) = with_state(|state| {
        for (principal, lender_info) in &state.lenders {
            if let Some(offer) = lender_info.loan_offers.iter().find(|o| o.offer_id == offer_id) {
                return Some((*principal, offer.clone()));
            }
        }
        None
    }).ok_or("Loan offer not found")?;

    validate_unsigned_tx(&unsigned_tx)?;

    if runes < loan_offer.runes_required {
        return Err("Borrower's collateral verification has failed".to_string());
    }

    let signed_tx = sign_transaction(&unsigned_tx)?;

    broadcast_transaction(&signed_tx).map_err(|e| e.to_string())?;

    with_state(|state| {
        if let Some(lender_info) = state.lenders.get_mut(&lender_principal) {
            if lender_info.balance < loan_offer.btc_amount {
                return Err("Not enough Lender balance to fulfill the loan".to_string());
            }
            lender_info.balance -= loan_offer.btc_amount;

            lender_info.loan_offers.retain(|o| o.offer_id != offer_id);
        } else {
            return Err("Lender not found".to_string());
        }

        state.active_loans.insert(
            offer_id,
            ActiveLoan {
                lender: lender_principal,
                borrower,
                btc_amount: loan_offer.btc_amount,
                collateral_amount: runes,
                status: LoanStatus::PendingConfirmation,
                signed_tx,
            },
        );

        Ok(())
    })?;

    Ok(())
}

#[query(name = "getLenderInfo")]
#[candid_method(query, rename = "getLenderInfo")]
fn get_lender_info() -> Option<LenderInfo> {
    let caller = ic_cdk::caller();
    with_state(|state| state.lenders.get(&caller).cloned())
}

#[query(name = "getLoanOffers")]
#[candid_method(query, rename = "getLoanOffers")]
fn get_loan_offers() -> Result<Vec<LoanOffer>, String> {
    let caller = ic_cdk::caller();

    with_state(|state| {
        let lender_info = state.lenders.get(&caller);
        match lender_info {
            Some(info) => Ok(info.loan_offers.clone()),
            None => Err("Lender not registered".to_string()),
        }
    })
}

#[query(name = "getAllLoanOffers")]
#[candid_method(query, rename = "getAllLoanOffers")]
fn get_all_loan_offers() -> Vec<LoanOffer> {
    with_state(|state| {
        state.lenders.values()
            .flat_map(|lender_info| lender_info.loan_offers.clone())
            .collect()
    })
}

#[query]
fn get_active_loans() -> Vec<ActiveLoan> {
    with_state(|state| state.active_loans.values().cloned().collect())
}

async fn generate_btc_address() -> String {
    format!("btc_address_{}", ic_cdk::api::time())
}

fn start_cron_job() {
    let interval = Duration::from_secs(60);
    ic_cdk_timers::set_timer_interval(interval, || {
        ic_cdk::spawn(async {
            monitor_btc_transactions().await;
        });
    });
}

async fn monitor_btc_transactions() {
    // Collect the principals to avoid borrowing issues
    let lender_principals = with_state(|state| state.lenders.keys().cloned().collect::<Vec<_>>());

    for principal in lender_principals {
        ic_cdk::spawn(process_lender_transactions(principal));
    }
}

async fn process_lender_transactions(principal: Principal) {
    // Extract lender data without holding the state borrow
    let (btc_address, pending_txs) = match with_state(|state| {
        state.lenders.get(&principal).map(|lender_info| {
            (
                lender_info.btc_address.clone(),
                lender_info.pending_txs.clone(),
            )
        })
    }) {
        Some(data) => data,
        None => return,
    };

    // Check for new transactions and update state
    if let Some(amount) = check_for_new_transactions(&btc_address).await {
        with_state(|state| {
            if let Some(lender_info) = state.lenders.get_mut(&principal) {
                lender_info.pending_txs.push(PendingTransaction {
                    amount,
                    confirmations: 0,
                });
            }
        });
    }

    let mut updated_pending_txs = Vec::new();
    let mut balance_increase = 0u64;

    for mut tx in pending_txs {
        if tx.confirmations >= 6 {
            // Transaction is confirmed
            balance_increase += tx.amount;
        } else if is_transaction_rejected().await {
            // Transaction is rejected
            handle_rejected_transaction(&btc_address).await;
        } else {
            // Increment confirmations
            tx.confirmations += 1;
            updated_pending_txs.push(tx);
        }
    }

    with_state(|state| {
        if let Some(lender_info) = state.lenders.get_mut(&principal) {
            lender_info.pending_txs = updated_pending_txs;
            lender_info.balance += balance_increase;
        }
    });
}

async fn check_for_new_transactions(_btc_address: &String) -> Option<u64> {
    // Generate a random byte
    let (random_bytes,): (Vec<u8>,) = call(Principal::management_canister(), "raw_rand", ()).await.unwrap();
    let random_value = random_bytes[0];

    // Simulate new BTC deposits with a 50% chance
    if random_value % 2 == 0 {
        // Generate a random amount between 1 and 100
        let amount = (random_value as u64 % 100) + 1;
        Some(amount)
    } else {
        None
    }
}

fn validate_unsigned_tx(unsigned_tx: &Vec<u8>) -> Result<(), String> {
    // Simulate decoding and validating the transaction
    // Assume the transaction is valid if the data is non-empty
    if unsigned_tx.is_empty() {
        return Err("Unsigned transaction is empty".to_string());
    }

    Ok(())
}

fn sign_transaction(unsigned_tx: &Vec<u8>) -> Result<Vec<u8>, String> {
    // Simulate signing the transaction
    let mut signed_tx = unsigned_tx.clone();
    signed_tx.extend_from_slice(b"signed");

    Ok(signed_tx)
}

fn broadcast_transaction(signed_tx: &Vec<u8>) -> Result<(), String> {
    // Simulate broadcasting the transaction to the network
    // Assume the transaction is successfully broadcasted
    ic_cdk::println!("Transaction broadcasted: {:?}", signed_tx);

    Ok(())
}

fn start_loan_monitoring_job() {
    let interval = Duration::from_secs(60);
    ic_cdk_timers::set_timer_interval(interval, || {
        ic_cdk::spawn(async {
            monitor_active_loans().await;
        });
    });
}

async fn monitor_active_loans() {
    let active_loan_ids = with_state(|state| state.active_loans.keys().cloned().collect::<Vec<_>>());

    for offer_id in active_loan_ids {
        ic_cdk::spawn(async move {
            process_active_loan(offer_id).await;
        });
    }
}

async fn process_active_loan(offer_id: u64) {
    let transaction_confirmed = check_transaction_status().await;

    with_state(|state| {
        if let Some(active_loan) = state.active_loans.get_mut(&offer_id) {
            match active_loan.status {
                LoanStatus::PendingConfirmation => {
                    if transaction_confirmed {
                        active_loan.status = LoanStatus::Confirmed;
                        ic_cdk::println!("Loan {} confirmed", offer_id);
                    } else {
                        active_loan.status = LoanStatus::Rejected;
                        ic_cdk::println!("Loan {} rejected", offer_id);

                        // Refund lender's balance
                        if let Some(lender_info) = state.lenders.get_mut(&active_loan.lender) {
                            lender_info.balance += active_loan.btc_amount;
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

async fn check_transaction_status() -> bool {
    // Generate a random byte
    let (random_bytes,): (Vec<u8>,) = call(Principal::management_canister(), "raw_rand", ()).await.unwrap();
    let random_value = random_bytes[0];

    random_value % 5 != 0 // 95% chance of confirmation
}

async fn is_transaction_rejected() -> bool {
    // Generate a random byte
    let (random_bytes,): (Vec<u8>,) = call(Principal::management_canister(), "raw_rand", ()).await.unwrap();
    let random_value = random_bytes[0];

    random_value % 20 == 0 // 5% chance of rejection
}

async fn handle_rejected_transaction(btc_address: &String) {
    ic_cdk::println!("Transaction rejected or dropped for address: {}", btc_address);
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;

    #[test]
    fn test_deposit_tracking() {
        let mut state = State::default();

        // Register lender
        let lender_principal = Principal::from_text("aaaaa-aa").unwrap();
        let btc_address = "btc_address_test".to_string();

        let lender_info = LenderInfo {
            btc_address: btc_address.clone(),
            balance: 0,
            pending_txs: Vec::new(),
            loan_offers: Vec::new(),
        };

        state.lenders.insert(lender_principal.clone(), lender_info);

        // Make deposit
        let amount_deposited = 1000;
        if let Some(lender_info) = state.lenders.get_mut(&lender_principal) {
            lender_info.balance += amount_deposited;
        }

        // Check lender's balance
        let lender_info = state.lenders.get(&lender_principal).expect("Lender info should exist");
        assert_eq!(
            lender_info.balance, amount_deposited,
            "Balance should be updated after deposit"
        );
    }

    #[test]
    fn test_create_loan_offer() {
        let mut state = State::default();
        let btc_amount = 100_000;

        // Register and make deposit for lender
        let lender_principal = Principal::from_text("aaaaa-aa").unwrap();
        let btc_address = "btc_address_test".to_string();

        let lender_info = LenderInfo {
            btc_address: btc_address.clone(),
            balance: 1_000_000,
            pending_txs: Vec::new(),
            loan_offers: Vec::new(),
        };

        state.lenders.insert(lender_principal.clone(), lender_info);

        // Create a loan offer
        let offer_id = state.next_offer_id;
        state.next_offer_id += 1;

        let loan_offer = LoanOffer {
            offer_id,
            btc_amount,
            runes_required: 100,
            max_ltv: 0.7,
        };

        if let Some(lender_info) = state.lenders.get_mut(&lender_principal) {
            lender_info.loan_offers.push(loan_offer.clone());
        }

        // Check loan offers
        let lender_info = state.lenders.get(&lender_principal).expect("Lender info should exist");
        assert_eq!(
            lender_info.loan_offers.len(),
            1,
            "There should be one loan offer"
        );
        assert_eq!(
            lender_info.loan_offers[0].offer_id, offer_id,
            "Offer ID should match"
        );
        assert_eq!(
            lender_info.loan_offers[0].btc_amount, btc_amount,
            "BTC amount should match"
        );
    }

    #[test]
    fn test_borrower_acceptance() {
        let mut state = State::default();
        let btc_amount = 500_000;

        // Register and make deposit for lender
        let lender_principal = Principal::from_text("aaaaa-aa").unwrap();
        let btc_address = "btc_address_test".to_string();

        let lender_info = LenderInfo {
            btc_address: btc_address.clone(),
            balance: 1_000_000,
            pending_txs: Vec::new(),
            loan_offers: Vec::new(),
        };

        state.lenders.insert(lender_principal.clone(), lender_info);

        // Create a loan offer
        let offer_id = state.next_offer_id;
        state.next_offer_id += 1;

        let loan_offer = LoanOffer {
            offer_id,
            btc_amount,
            runes_required: 100,
            max_ltv: 0.7,
        };

        if let Some(lender_info) = state.lenders.get_mut(&lender_principal) {
            lender_info.loan_offers.push(loan_offer.clone());
        }

        // Simulate borrower accepting the loan offer
        let borrower_principal = Principal::from_text("2vxsx-fae").unwrap();
        let runes = 100;

        let (found_lender_principal, found_loan_offer) = {
            let mut found = None;
            for (principal, lender_info) in &state.lenders {
                if let Some(offer) = lender_info.loan_offers.iter().find(|o| o.offer_id == offer_id) {
                    found = Some((principal.clone(), offer.clone()));
                    break;
                }
            }
            found.expect("Loan offer not found")
        };

        assert!(
            runes >= found_loan_offer.runes_required,
            "Insufficient collateral"
        );

        if let Some(lender_info) = state.lenders.get_mut(&found_lender_principal) {
            assert!(
                lender_info.balance >= found_loan_offer.btc_amount,
                "Not enough Lender balance to fulfill the loan"
            );
            lender_info.balance -= found_loan_offer.btc_amount;
            lender_info.loan_offers.retain(|o| o.offer_id != offer_id);
        }

        state.active_loans.insert(
            offer_id,
            ActiveLoan {
                lender: found_lender_principal.clone(),
                borrower: borrower_principal.clone(),
                btc_amount: found_loan_offer.btc_amount,
                collateral_amount: runes,
                status: LoanStatus::PendingConfirmation,
                signed_tx: vec![], // Some static mock value
            },
        );

        let lender_info = state
            .lenders
            .get(&found_lender_principal)
            .expect("Lender info should exist");
        assert_eq!(
            lender_info.balance, btc_amount,
            "Lender's balance should be deducted by the loan amount"
        );

        assert!(
            lender_info.loan_offers.is_empty(),
            "Loan offer should be removed"
        );

        assert!(
            state.active_loans.contains_key(&offer_id),
            "Active loan should be recorded"
        );
    }
}
