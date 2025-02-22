use crate::checked_amount::CheckedAmountOf;
use crate::endpoints::CandidBlockTag;
use crate::eth_logs::{EventSource, ReceivedEthEvent};
use crate::eth_rpc::{BlockTag, Hash};
use crate::eth_rpc_client::responses::{TransactionReceipt, TransactionStatus};
use crate::lifecycle::init::InitArg;
use crate::lifecycle::upgrade::UpgradeArg;
use crate::lifecycle::EthereumNetwork;
use crate::numeric::{
    wei_from_milli_ether, BlockNumber, GasAmount, LedgerBurnIndex, LedgerMintIndex, LogIndex,
    TransactionNonce, Wei, WeiPerGas,
};
use crate::state::event::{Event, EventType};
use crate::state::State;
use crate::tx::{
    AccessList, AccessListItem, Eip1559Signature, Eip1559TransactionRequest,
    SignedEip1559TransactionRequest, StorageKey,
};
use candid::{Nat, Principal};
use ethnum::u256;
use ic_ethereum_types::Address;
use proptest::array::{uniform20, uniform32};
use proptest::collection::vec as pvec;
use proptest::prelude::*;

mod next_request_id {
    use crate::state::tests::a_state;

    #[test]
    fn should_retrieve_and_increment_counter() {
        let mut state = a_state();

        assert_eq!(state.next_request_id(), 0);
        assert_eq!(state.next_request_id(), 1);
        assert_eq!(state.next_request_id(), 2);
        assert_eq!(state.next_request_id(), 3);
    }

    #[test]
    fn should_wrap_to_0_when_overflow() {
        let mut state = a_state();
        state.http_request_counter = u64::MAX;

        assert_eq!(state.next_request_id(), u64::MAX);
        assert_eq!(state.next_request_id(), 0);
    }
}

fn a_state() -> State {
    State::try_from(InitArg {
        ethereum_network: Default::default(),
        ecdsa_key_name: "test_key_1".to_string(),
        ethereum_contract_address: None,
        ledger_id: Principal::from_text("apia6-jaaaa-aaaar-qabma-cai")
            .expect("BUG: invalid principal"),
        ethereum_block_height: Default::default(),
        minimum_withdrawal_amount: wei_from_milli_ether(10).into(),
        next_transaction_nonce: Default::default(),
        last_scraped_block_number: Default::default(),
    })
    .expect("init args should be valid")
}

mod mint_transaction {
    use crate::eth_logs::{EventSourceError, ReceivedEthEvent};
    use crate::lifecycle::init::InitArg;
    use crate::numeric::{wei_from_milli_ether, LedgerMintIndex, LogIndex};
    use crate::state::tests::received_eth_event;
    use crate::state::{MintedEvent, State};

    #[test]
    fn should_record_mint_task_from_event() {
        let mut state = dummy_state();
        let event = received_eth_event();

        state.record_event_to_mint(&event);

        assert!(state.events_to_mint.contains_key(&event.source()));

        let block_index = LedgerMintIndex::new(1u64);

        let minted_event = MintedEvent {
            deposit_event: event.clone(),
            mint_block_index: block_index,
        };

        state.record_successful_mint(event.source(), block_index);

        assert!(!state.events_to_mint.contains_key(&event.source()));
        assert_eq!(
            state.minted_events.get(&event.source()),
            Some(&minted_event)
        );
    }

    #[test]
    fn should_allow_minting_events_with_equal_txhash() {
        let mut state = dummy_state();
        let event_1 = ReceivedEthEvent {
            log_index: LogIndex::from(1u8),
            ..received_eth_event()
        };
        let event_2 = ReceivedEthEvent {
            log_index: LogIndex::from(2u8),
            ..received_eth_event()
        };

        assert_ne!(event_1, event_2);

        state.record_event_to_mint(&event_1);

        assert!(state.events_to_mint.contains_key(&event_1.source()));

        state.record_event_to_mint(&event_2);

        assert!(state.events_to_mint.contains_key(&event_2.source()));

        assert_eq!(2, state.events_to_mint.len());
    }

    #[test]
    #[should_panic = "unknown event"]
    fn should_not_allow_unknown_mints() {
        let mut state = dummy_state();
        let event = received_eth_event();

        assert!(!state.events_to_mint.contains_key(&event.source()));
        state.record_successful_mint(event.source(), LedgerMintIndex::new(1));
    }

    #[test]
    #[should_panic = "invalid"]
    fn should_not_record_invalid_deposit_already_recorded_as_valid() {
        let mut state = dummy_state();
        let event = received_eth_event();

        state.record_event_to_mint(&event);

        assert!(state.events_to_mint.contains_key(&event.source()));

        state.record_invalid_deposit(
            event.source(),
            EventSourceError::InvalidEvent("bad".to_string()).to_string(),
        );
    }

    #[test]
    fn should_not_update_already_recorded_invalid_deposit() {
        let mut state = dummy_state();
        let event = received_eth_event();
        let error = EventSourceError::InvalidEvent("first".to_string());
        let other_error = EventSourceError::InvalidEvent("second".to_string());
        assert_ne!(error, other_error);

        assert!(state.record_invalid_deposit(event.source(), error.to_string()));
        assert_eq!(state.invalid_events[&event.source()], error.to_string());

        assert!(!state.record_invalid_deposit(event.source(), other_error.to_string()));
        assert_eq!(state.invalid_events[&event.source()], error.to_string());
    }

    #[test]
    fn should_have_readable_debug_representation() {
        let expected = "ReceivedEthEvent { \
          transaction_hash: 0xf1ac37d920fa57d9caeebc7136fea591191250309ffca95ae0e8a7739de89cc2, \
          block_number: 3_960_623, \
          log_index: 29, \
          from_address: 0xdd2851Cdd40aE6536831558DD46db62fAc7A844d, \
          value: 10_000_000_000_000_000, \
          principal: k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae \
        }";
        assert_eq!(format!("{:?}", received_eth_event()), expected);
    }

    fn dummy_state() -> State {
        use candid::Principal;
        State::try_from(InitArg {
            ethereum_network: Default::default(),
            ecdsa_key_name: "test_key_1".to_string(),
            ethereum_contract_address: None,
            ledger_id: Principal::from_text("apia6-jaaaa-aaaar-qabma-cai")
                .expect("BUG: invalid principal"),
            ethereum_block_height: Default::default(),
            minimum_withdrawal_amount: wei_from_milli_ether(10).into(),
            next_transaction_nonce: Default::default(),
            last_scraped_block_number: Default::default(),
        })
        .expect("init args should be valid")
    }
}

fn received_eth_event() -> ReceivedEthEvent {
    ReceivedEthEvent {
        transaction_hash: "0xf1ac37d920fa57d9caeebc7136fea591191250309ffca95ae0e8a7739de89cc2"
            .parse()
            .unwrap(),
        block_number: BlockNumber::new(3960623u128),
        log_index: LogIndex::from(29u8),
        from_address: "0xdd2851cdd40ae6536831558dd46db62fac7a844d"
            .parse()
            .unwrap(),
        value: Wei::from(10_000_000_000_000_000_u128),
        principal: "k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae"
            .parse()
            .unwrap(),
    }
}

mod upgrade {
    use crate::eth_rpc::BlockTag;
    use crate::lifecycle::upgrade::UpgradeArg;
    use crate::numeric::{wei_from_milli_ether, TransactionNonce, Wei};
    use crate::state::{InvalidStateError, State};
    use assert_matches::assert_matches;
    use candid::Nat;
    use ic_ethereum_types::Address;
    use num_bigint::BigUint;
    use std::str::FromStr;

    #[test]
    fn should_fail_when_upgrade_args_invalid() {
        let mut state = initial_state();
        assert_matches!(
            state.upgrade(UpgradeArg {
                next_transaction_nonce: Some(Nat(BigUint::from_bytes_be(
                    &ethnum::u256::MAX.to_be_bytes(),
                ) + 1_u8)),
                ..Default::default()
            }),
            Err(InvalidStateError::InvalidTransactionNonce(_))
        );

        let mut state = initial_state();
        assert_matches!(
            state.upgrade(UpgradeArg {
                minimum_withdrawal_amount: Some(Nat::from(0_u8)),
                ..Default::default()
            }),
            Err(InvalidStateError::InvalidMinimumWithdrawalAmount(_))
        );

        let mut state = initial_state();
        assert_matches!(
            state.upgrade(UpgradeArg {
                ethereum_contract_address: Some("invalid".to_string()),
                ..Default::default()
            }),
            Err(InvalidStateError::InvalidEthereumContractAddress(_))
        );

        let mut state = initial_state();
        assert_matches!(
            state.upgrade(UpgradeArg {
                ethereum_contract_address: Some(
                    "0x0000000000000000000000000000000000000000".to_string(),
                ),
                ..Default::default()
            }),
            Err(InvalidStateError::InvalidEthereumContractAddress(_))
        );
    }

    #[test]
    fn should_succeed() {
        use crate::endpoints::CandidBlockTag;
        let mut state = initial_state();
        let upgrade_arg = UpgradeArg {
            next_transaction_nonce: Some(Nat::from(15_u8)),
            minimum_withdrawal_amount: Some(Nat::from(100_u8)),
            ethereum_contract_address: Some(
                "0xb44B5e756A894775FC32EDdf3314Bb1B1944dC34".to_string(),
            ),
            ethereum_block_height: Some(CandidBlockTag::Safe),
        };

        state.upgrade(upgrade_arg).expect("valid upgrade args");

        assert_eq!(
            state.eth_transactions.next_transaction_nonce(),
            TransactionNonce::from(15_u64)
        );
        assert_eq!(state.minimum_withdrawal_amount, Wei::from(100_u64));
        assert_eq!(
            state.ethereum_contract_address,
            Some(Address::from_str("0xb44B5e756A894775FC32EDdf3314Bb1B1944dC34").unwrap())
        );
        assert_eq!(state.ethereum_block_height, BlockTag::Safe);
    }

    fn initial_state() -> State {
        use crate::lifecycle::init::InitArg;
        use candid::Principal;
        State::try_from(InitArg {
            ethereum_network: Default::default(),
            ecdsa_key_name: "test_key_1".to_string(),
            ethereum_contract_address: None,
            ledger_id: Principal::from_text("apia6-jaaaa-aaaar-qabma-cai")
                .expect("BUG: invalid principal"),
            ethereum_block_height: Default::default(),
            minimum_withdrawal_amount: wei_from_milli_ether(10).into(),
            next_transaction_nonce: Default::default(),
            last_scraped_block_number: Default::default(),
        })
        .expect("valid init args")
    }
}

fn arb_hash() -> impl Strategy<Value = Hash> {
    uniform32(any::<u8>()).prop_map(Hash)
}

fn arb_address() -> impl Strategy<Value = Address> {
    uniform20(any::<u8>()).prop_map(Address::new)
}

fn arb_principal() -> impl Strategy<Value = Principal> {
    pvec(any::<u8>(), 0..=29).prop_map(|bytes| Principal::from_slice(&bytes))
}

fn arb_u256() -> impl Strategy<Value = u256> {
    uniform32(any::<u8>()).prop_map(u256::from_be_bytes)
}

fn arb_checked_amount_of<Unit>() -> impl Strategy<Value = CheckedAmountOf<Unit>> {
    (any::<u128>(), any::<u128>()).prop_map(|(hi, lo)| CheckedAmountOf::from_words(hi, lo))
}

fn arb_event_source() -> impl Strategy<Value = EventSource> {
    (arb_hash(), arb_checked_amount_of()).prop_map(|(transaction_hash, log_index)| EventSource {
        transaction_hash,
        log_index,
    })
}

fn arb_block_tag() -> impl Strategy<Value = CandidBlockTag> {
    prop_oneof![
        Just(CandidBlockTag::Safe),
        Just(CandidBlockTag::Latest),
        Just(CandidBlockTag::Finalized),
    ]
}

fn arb_nat() -> impl Strategy<Value = Nat> {
    any::<u128>().prop_map(Nat::from)
}

fn arb_storage_key() -> impl Strategy<Value = StorageKey> {
    uniform32(any::<u8>()).prop_map(StorageKey)
}
fn arb_access_list_item() -> impl Strategy<Value = AccessListItem> {
    (arb_address(), pvec(arb_storage_key(), 0..100)).prop_map(|(address, storage_keys)| {
        AccessListItem {
            address,
            storage_keys,
        }
    })
}
fn arb_access_list() -> impl Strategy<Value = AccessList> {
    pvec(arb_access_list_item(), 0..100).prop_map(AccessList)
}

prop_compose! {
    fn arb_init_arg()(
        contract_address in proptest::option::of(arb_address()),
        ethereum_block_height in arb_block_tag(),
        minimum_withdrawal_amount in arb_nat(),
        next_transaction_nonce in arb_nat(),
        ledger_id in arb_principal(),
        ecdsa_key_name in "[a-z_]*",
        last_scraped_block_number in arb_nat(),
    ) -> InitArg {
        InitArg {
            ethereum_network: EthereumNetwork::Sepolia,
            ecdsa_key_name,
            ethereum_contract_address: contract_address.map(|addr| addr.to_string()),
            ledger_id,
            ethereum_block_height,
            minimum_withdrawal_amount,
            next_transaction_nonce,
            last_scraped_block_number
        }
    }
}

prop_compose! {
    fn arb_upgrade_arg()(
        contract_address in proptest::option::of(arb_address()),
        ethereum_block_height in proptest::option::of(arb_block_tag()),
        minimum_withdrawal_amount in proptest::option::of(arb_nat()),
        next_transaction_nonce in proptest::option::of(arb_nat()),
    ) -> UpgradeArg {
        UpgradeArg {
            ethereum_contract_address: contract_address.map(|addr| addr.to_string()),
            ethereum_block_height,
            minimum_withdrawal_amount,
            next_transaction_nonce,
        }
    }
}

prop_compose! {
    fn arb_received_eth_event()(
        transaction_hash in arb_hash(),
        block_number in arb_checked_amount_of(),
        log_index in arb_checked_amount_of(),
        from_address in arb_address(),
        value in arb_checked_amount_of(),
        principal in arb_principal(),
    ) -> ReceivedEthEvent {
        ReceivedEthEvent {
            transaction_hash,
            block_number,
            log_index,
            from_address,
            value,
            principal,
        }
    }
}

prop_compose! {
    fn arb_unsigned_tx()(
        chain_id in any::<u64>(),
        nonce in arb_checked_amount_of(),
        max_priority_fee_per_gas in arb_checked_amount_of(),
        max_fee_per_gas in arb_checked_amount_of(),
        gas_limit in arb_checked_amount_of(),
        destination in arb_address(),
        amount in arb_checked_amount_of(),
        data in pvec(any::<u8>(), 0..20),
        access_list in arb_access_list(),
    ) -> Eip1559TransactionRequest {
         Eip1559TransactionRequest {
            chain_id,
            nonce,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            gas_limit,
            destination,
            amount,
            data,
            access_list,
        }
    }
}

prop_compose! {
    fn arb_signed_tx()(
        unsigned_tx in arb_unsigned_tx(),
        r in arb_u256(),
        s in arb_u256(),
        signature_y_parity in any::<bool>(),
    ) -> SignedEip1559TransactionRequest {
        SignedEip1559TransactionRequest::from((
            unsigned_tx,
            Eip1559Signature {
                r,
                s,
                signature_y_parity,
            }
        ))
    }
}

fn arb_transaction_status() -> impl Strategy<Value = TransactionStatus> {
    prop_oneof![
        Just(TransactionStatus::Success),
        Just(TransactionStatus::Failure),
    ]
}

prop_compose! {
    fn arb_tx_receipt()(
        block_hash in arb_hash(),
        block_number in arb_checked_amount_of(),
        effective_gas_price in arb_checked_amount_of(),
        gas_used in arb_checked_amount_of(),
        status in arb_transaction_status(),
        transaction_hash in arb_hash(),
    ) -> TransactionReceipt {
        TransactionReceipt {
            block_hash,
            block_number,
            effective_gas_price,
            gas_used,
            status,
            transaction_hash,
        }
    }
}

fn arb_event_type() -> impl Strategy<Value = EventType> {
    prop_oneof![
        arb_init_arg().prop_map(EventType::Init),
        arb_upgrade_arg().prop_map(EventType::Upgrade),
        arb_received_eth_event().prop_map(EventType::AcceptedDeposit),
        arb_event_source().prop_map(|event_source| EventType::InvalidDeposit {
            event_source,
            reason: "bad principal".to_string()
        }),
        (arb_event_source(), any::<u64>()).prop_map(|(event_source, index)| {
            EventType::MintedCkEth {
                event_source,
                mint_block_index: index.into(),
            }
        }),
        arb_checked_amount_of().prop_map(|block_number| EventType::SyncedToBlock { block_number }),
        (any::<u64>(), arb_unsigned_tx()).prop_map(|(withdrawal_id, transaction)| {
            EventType::CreatedTransaction {
                withdrawal_id: withdrawal_id.into(),
                transaction,
            }
        }),
        (any::<u64>(), arb_signed_tx()).prop_map(|(withdrawal_id, transaction)| {
            EventType::SignedTransaction {
                withdrawal_id: withdrawal_id.into(),
                transaction,
            }
        }),
        (any::<u64>(), arb_unsigned_tx()).prop_map(|(withdrawal_id, transaction)| {
            EventType::ReplacedTransaction {
                withdrawal_id: withdrawal_id.into(),
                transaction,
            }
        }),
        (any::<u64>(), arb_tx_receipt()).prop_map(|(withdrawal_id, transaction_receipt)| {
            EventType::FinalizedTransaction {
                withdrawal_id: withdrawal_id.into(),
                transaction_receipt,
            }
        }),
    ]
}

fn arb_event() -> impl Strategy<Value = Event> {
    (any::<u64>(), arb_event_type()).prop_map(|(timestamp, payload)| Event { timestamp, payload })
}

proptest! {
    #[test]
    fn event_encoding_roundtrip(event in arb_event()) {
        use ic_stable_structures::storable::Storable;
        let bytes = event.to_bytes();
        prop_assert_eq!(&event, &Event::from_bytes(bytes.clone()), "failed to decode bytes {}", hex::encode(bytes));
    }
}

#[test]
fn state_equivalence() {
    use crate::eth_rpc_client::responses::{TransactionReceipt, TransactionStatus};
    use crate::map::MultiKeyMap;
    use crate::state::transactions::{
        EthTransactions, EthWithdrawalRequest, Reimbursed, ReimbursementRequest,
    };
    use crate::state::MintedEvent;
    use crate::tx::{Eip1559Signature, Eip1559TransactionRequest};
    use ic_cdk::api::management_canister::ecdsa::EcdsaPublicKeyResponse;
    use maplit::btreemap;

    fn source(txhash: &str, index: u64) -> EventSource {
        EventSource {
            transaction_hash: txhash.parse().unwrap(),
            log_index: LogIndex::from(index),
        }
    }

    fn singleton_map<T: std::fmt::Debug>(
        nonce: u128,
        burn_index: u64,
        value: T,
    ) -> MultiKeyMap<TransactionNonce, LedgerBurnIndex, T> {
        let mut map = MultiKeyMap::new();
        map.try_insert(
            TransactionNonce::new(nonce),
            LedgerBurnIndex::new(burn_index),
            value,
        )
        .unwrap();
        map
    }

    let withdrawal_request1 = EthWithdrawalRequest {
        withdrawal_amount: Wei::new(10_999_968_499_999_664_000),
        destination: "0xA776Cc20DFdCCF0c3ba89cB9Fb0f10Aba5b98f52"
            .parse()
            .unwrap(),
        ledger_burn_index: LedgerBurnIndex::new(10),
        from: "2chl6-4hpzw-vqaaa-aaaaa-c".parse().unwrap(),
        from_subaccount: None,
        created_at: Some(1699527697000000000),
    };
    let withdrawal_request2 = EthWithdrawalRequest {
        ledger_burn_index: LedgerBurnIndex::new(20),
        ..withdrawal_request1.clone()
    };
    let eth_transactions = EthTransactions {
        withdrawal_requests: vec![withdrawal_request1.clone(), withdrawal_request2.clone()]
            .into_iter()
            .collect(),
        created_tx: singleton_map(
            2,
            4,
            Eip1559TransactionRequest {
                chain_id: 1,
                nonce: TransactionNonce::new(2),
                max_priority_fee_per_gas: WeiPerGas::new(100_000_000),
                max_fee_per_gas: WeiPerGas::new(100_000_000),
                gas_limit: GasAmount::new(21_000),
                destination: "0xA776Cc20DFdCCF0c3ba89cB9Fb0f10Aba5b98f52"
                    .parse()
                    .unwrap(),
                amount: Wei::new(1_000_000_000_000),
                data: vec![],
                access_list: Default::default(),
            },
        ),
        sent_tx: singleton_map(
            1,
            3,
            vec![SignedEip1559TransactionRequest::from((
                Eip1559TransactionRequest {
                    chain_id: 1,
                    nonce: TransactionNonce::new(1),
                    max_priority_fee_per_gas: WeiPerGas::new(100_000_000),
                    max_fee_per_gas: WeiPerGas::new(100_000_000),
                    gas_limit: GasAmount::new(21_000),
                    destination: "0xA776Cc20DFdCCF0c3ba89cB9Fb0f10Aba5b98f52"
                        .parse()
                        .unwrap(),
                    amount: Wei::new(1_000_000_000_000),
                    data: vec![],
                    access_list: Default::default(),
                },
                Eip1559Signature {
                    signature_y_parity: true,
                    r: Default::default(),
                    s: Default::default(),
                },
            ))],
        ),
        finalized_tx: singleton_map(
            0,
            2,
            SignedEip1559TransactionRequest::from((
                Eip1559TransactionRequest {
                    chain_id: 1,
                    nonce: TransactionNonce::new(0),
                    max_priority_fee_per_gas: WeiPerGas::new(100_000_000),
                    max_fee_per_gas: WeiPerGas::new(100_000_000),
                    gas_limit: GasAmount::new(21_000),
                    destination: "0xA776Cc20DFdCCF0c3ba89cB9Fb0f10Aba5b98f52"
                        .parse()
                        .unwrap(),
                    amount: Wei::new(1_000_000_000_000),
                    data: vec![],
                    access_list: Default::default(),
                },
                Eip1559Signature {
                    signature_y_parity: true,
                    r: Default::default(),
                    s: Default::default(),
                },
            ))
            .try_finalize(TransactionReceipt {
                block_hash: "0x9e1e2124a453e7b5afaabe42fb66fffb12d4b1053403d2f487d250007f3cb550"
                    .parse()
                    .unwrap(),
                block_number: BlockNumber::new(400_000),
                effective_gas_price: WeiPerGas::new(100_000_000),
                gas_used: GasAmount::new(21_000),
                status: TransactionStatus::Success,
                transaction_hash:
                    "0x06afc3c693dc2ba2c19b5c287c4dddce040d766bea5fd13c8a7268b04aa94f2d"
                        .parse()
                        .unwrap(),
            })
            .expect("valid receipt"),
        ),
        next_nonce: TransactionNonce::new(3),
        maybe_reimburse: btreemap! {
            LedgerBurnIndex::new(4) => EthWithdrawalRequest {
                withdrawal_amount: Wei::new(1_000_000_000_000),
                ledger_burn_index: LedgerBurnIndex::new(4),
                destination: "0xA776Cc20DFdCCF0c3ba89cB9Fb0f10Aba5b98f52".parse().unwrap(),
                from: "ezu3d-2mifu-k3bh4-oqhrj-mbrql-5p67r-pp6pr-dbfra-unkx5-sxdtv-rae"
                    .parse()
                    .unwrap(),
                from_subaccount: None,
                created_at: Some(1699527697000000000),
            }
        },
        reimbursement_requests: btreemap! {
            LedgerBurnIndex::new(3) => ReimbursementRequest {
                transaction_hash: Some("0x06afc3c693dc2ba2c19b5c287c4dddce040d766bea5fd13c8a7268b04aa94f2d"
                .parse()
                .unwrap()),
                withdrawal_id: LedgerBurnIndex::new(3),
                reimbursed_amount: Wei::new(100_000_000_000),
                to: "ezu3d-2mifu-k3bh4-oqhrj-mbrql-5p67r-pp6pr-dbfra-unkx5-sxdtv-rae".parse().unwrap(),
                to_subaccount: None,
            }
        },
        reimbursed: btreemap! {
            LedgerBurnIndex::new(6) => Reimbursed {
                transaction_hash: Some("0x06afc3c693dc2ba2c19b5c287c4dddce040d766bea5fd13c8a7268b04aa94f2d".parse().unwrap()),
                reimbursed_in_block: LedgerMintIndex::new(150),
                reimbursed_amount: Wei::new(10_000_000_000_000),
                withdrawal_id: LedgerBurnIndex::new(6),
            },
        },
    };
    let state = State {
        ethereum_network: EthereumNetwork::Mainnet,
        ecdsa_key_name: "test_key".to_string(),
        ledger_id: "apia6-jaaaa-aaaar-qabma-cai".parse().unwrap(),
        ethereum_contract_address: Some(
            "0xb44B5e756A894775FC32EDdf3314Bb1B1944dC34"
                .parse()
                .unwrap(),
        ),
        ecdsa_public_key: Some(EcdsaPublicKeyResponse {
            public_key: vec![1; 32],
            chain_code: vec![2; 32],
        }),
        minimum_withdrawal_amount: Wei::new(1_000_000_000_000_000),
        ethereum_block_height: BlockTag::Finalized,
        first_scraped_block_number: BlockNumber::new(1_000_001),
        last_scraped_block_number: BlockNumber::new(1_000_000),
        last_observed_block_number: Some(BlockNumber::new(2_000_000)),
        events_to_mint: btreemap! {
            source("0xac493fb20c93bd3519a4a5d90ce72d69455c41c5b7e229dafee44344242ba467", 100) => ReceivedEthEvent {
                transaction_hash: "0xac493fb20c93bd3519a4a5d90ce72d69455c41c5b7e229dafee44344242ba467".parse().unwrap(),
                block_number: BlockNumber::new(500_000),
                log_index: LogIndex::new(100),
                from_address: "0x9d68bd6F351bE62ed6dBEaE99d830BECD356Ed25".parse().unwrap(),
                value: Wei::new(500_000_000_000_000_000),
                principal: "lsywz-sl5vm-m6tct-7fhwt-6gdrw-4uzsg-ibknl-44d6d-a2oyt-c2cxu-7ae".parse().unwrap(),
            }
        },
        minted_events: btreemap! {
            source("0x705f826861c802b407843e99af986cfde8749b669e5e0a5a150f4350bcaa9bc3", 1) => MintedEvent {
                deposit_event: ReceivedEthEvent {
                    transaction_hash: "0x705f826861c802b407843e99af986cfde8749b669e5e0a5a150f4350bcaa9bc3".parse().unwrap(),
                    block_number: BlockNumber::new(450_000),
                    log_index: LogIndex::new(1),
                    from_address: "0x9d68bd6F351bE62ed6dBEaE99d830BECD356Ed25".parse().unwrap(),
                    value: Wei::new(10_000_000_000_000_000),
                    principal: "2chl6-4hpzw-vqaaa-aaaaa-c".parse().unwrap(),
                },
                mint_block_index: LedgerMintIndex::new(1),
            }
        },
        invalid_events: btreemap! {
            source("0x05c6ec45699c9a6a4b1a4ea2058b0cee852ea2f19b18fb8313c04bf8156efde4", 11) => "failed to decode principal from bytes 0x00333c125dc9f41abaf2b8b85d49fdc7ff75b2a4000000000000000000000000".to_string(),
        },
        eth_transactions: eth_transactions.clone(),
        retrieve_eth_principals: Default::default(),
        active_tasks: Default::default(),
        http_request_counter: 100,
        eth_balance: Default::default(),
        skipped_blocks: Default::default(),
    };

    assert_eq!(
        Ok(()),
        state.is_equivalent_to(&State {
            ecdsa_public_key: None,
            last_observed_block_number: None,
            http_request_counter: 0,
            ..state.clone()
        }),
        "changing only computed/transient fields should result in an equivalent state",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            first_scraped_block_number: BlockNumber::new(100_000_000_000),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            last_scraped_block_number: BlockNumber::new(100_000_000_000),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            ecdsa_key_name: "".to_string(),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            ethereum_contract_address: None,
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            minimum_withdrawal_amount: Wei::new(1),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            ethereum_block_height: BlockTag::Latest,
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            events_to_mint: Default::default(),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            minted_events: Default::default(),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            invalid_events: Default::default(),
            ..state.clone()
        }),
        "changing essential fields should break equivalence",
    );

    assert_eq!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                withdrawal_requests: vec![withdrawal_request2.clone(), withdrawal_request1.clone()]
                    .into_iter()
                    .collect(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        },),
        "changing the order of withdrawal requests should result in an equivalent state",
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                withdrawal_requests: vec![withdrawal_request1].into_iter().collect(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the withdrawal requests should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                sent_tx: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the transactions should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                created_tx: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the transactions should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                finalized_tx: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the transactions should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                maybe_reimburse: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the reimbursement data should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                reimbursement_requests: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the reimbursement data should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                reimbursed: Default::default(),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the reimbursement data should break equivalence"
    );

    assert_ne!(
        Ok(()),
        state.is_equivalent_to(&State {
            eth_transactions: EthTransactions {
                next_nonce: TransactionNonce::new(1000),
                ..eth_transactions.clone()
            },
            ..state.clone()
        }),
        "changing the next nonce should break equivalence"
    );
}

mod eth_balance {
    use crate::eth_rpc_client::responses::{TransactionReceipt, TransactionStatus};
    use crate::lifecycle::EthereumNetwork;
    use crate::numeric::{
        BlockNumber, GasAmount, LedgerBurnIndex, TransactionNonce, Wei, WeiPerGas,
    };
    use crate::state::audit::{apply_state_transition, EventType};
    use crate::state::tests::{a_state, received_eth_event};
    use crate::state::transactions::EthWithdrawalRequest;
    use crate::state::{EthBalance, State};
    use crate::tx::{
        Eip1559Signature, Eip1559TransactionRequest, SignedEip1559TransactionRequest,
        TransactionPrice,
    };

    #[test]
    fn should_add_deposit_to_eth_balance() {
        let mut state = a_state();
        let balance_before = state.eth_balance.clone();

        let deposit_event = received_eth_event();
        apply_state_transition(
            &mut state,
            &EventType::AcceptedDeposit(deposit_event.clone()),
        );
        let balance_after = state.eth_balance.clone();

        assert_eq!(
            balance_after,
            EthBalance {
                eth_balance: deposit_event.value,
                ..balance_before
            }
        )
    }

    #[test]
    fn should_ignore_rejected_deposit() {
        let mut state = a_state();
        let balance_before = state.eth_balance.clone();

        let deposit_event = received_eth_event();
        apply_state_transition(
            &mut state,
            &EventType::InvalidDeposit {
                event_source: deposit_event.source(),
                reason: "invalid principal".to_string(),
            },
        );
        let balance_after = state.eth_balance.clone();

        assert_eq!(balance_after, balance_before)
    }

    #[test]
    fn should_update_after_successful_and_failed_withdrawal() {
        let mut state_before_withdrawal = a_state();
        apply_state_transition(
            &mut state_before_withdrawal,
            &EventType::AcceptedDeposit(received_eth_event()),
        );

        let mut state_after_successful_withdrawal = state_before_withdrawal.clone();
        let balance_before_withdrawal = state_after_successful_withdrawal.eth_balance.clone();
        //Values from https://sepolia.etherscan.io/tx/0xef628b8f45984bdf386f5b765b665a2e584295e1190d21c6acdfabe17c27e1bb
        let withdrawal_flow = WithdrawalFlow {
            withdrawal_amount: Wei::new(10_000_000_000_000_000),
            tx_fee: TransactionPrice {
                gas_limit: GasAmount::from(21_000_u32),
                max_fee_per_gas: WeiPerGas::from(7_828_365_474_u64),
                max_priority_fee_per_gas: WeiPerGas::from(1_500_000_000_u64),
            },
            effective_gas_price: WeiPerGas::from(0x1176e9eb9_u64),
            tx_status: TransactionStatus::Success,
            ..Default::default()
        };
        withdrawal_flow
            .clone()
            .apply(&mut state_after_successful_withdrawal);
        let balance_after_successful_withdrawal =
            state_after_successful_withdrawal.eth_balance.clone();

        assert_eq!(
            balance_after_successful_withdrawal,
            EthBalance {
                eth_balance: balance_before_withdrawal
                    .eth_balance
                    .checked_sub(Wei::from(9_934_054_275_043_000_u64))
                    .unwrap(),
                total_effective_tx_fees: balance_before_withdrawal
                    .total_effective_tx_fees
                    .checked_add(Wei::from(98_449_949_997_000_u64))
                    .unwrap(),
                total_unspent_tx_fees: balance_before_withdrawal
                    .total_unspent_tx_fees
                    .checked_add(Wei::from(65_945_724_957_000_u64))
                    .unwrap()
            }
        );

        let mut state_after_failed_withdrawal = state_before_withdrawal.clone();
        let receipt_failed = WithdrawalFlow {
            tx_status: TransactionStatus::Failure,
            ..withdrawal_flow
        }
        .apply(&mut state_after_failed_withdrawal);
        let balance_after_failed_withdrawal = state_after_failed_withdrawal.eth_balance.clone();

        assert_eq!(
            balance_after_failed_withdrawal.eth_balance,
            balance_before_withdrawal
                .eth_balance
                .checked_sub(receipt_failed.effective_transaction_fee())
                .unwrap()
        );
        assert_eq!(
            balance_after_successful_withdrawal.total_effective_tx_fees,
            balance_after_failed_withdrawal.total_effective_tx_fees
        );
        assert_eq!(
            balance_after_successful_withdrawal.total_unspent_tx_fees,
            balance_after_failed_withdrawal.total_unspent_tx_fees()
        );
    }

    #[derive(Clone)]
    struct WithdrawalFlow {
        ledger_burn_index: LedgerBurnIndex,
        nonce: TransactionNonce,
        withdrawal_amount: Wei,
        tx_fee: TransactionPrice,
        effective_gas_price: WeiPerGas,
        tx_status: TransactionStatus,
    }

    impl Default for WithdrawalFlow {
        fn default() -> Self {
            Self {
                ledger_burn_index: LedgerBurnIndex::new(0),
                nonce: TransactionNonce::ZERO,
                withdrawal_amount: Wei::ONE,
                tx_fee: TransactionPrice {
                    gas_limit: GasAmount::from(21_000_u32),
                    max_fee_per_gas: WeiPerGas::ONE,
                    max_priority_fee_per_gas: WeiPerGas::ONE,
                },
                effective_gas_price: WeiPerGas::ONE,
                tx_status: TransactionStatus::Success,
            }
        }
    }

    impl WithdrawalFlow {
        fn apply(self, state: &mut State) -> TransactionReceipt {
            let withdrawal_request = EthWithdrawalRequest {
                withdrawal_amount: self.withdrawal_amount,
                destination: "0xb44B5e756A894775FC32EDdf3314Bb1B1944dC34"
                    .parse()
                    .unwrap(),
                ledger_burn_index: self.ledger_burn_index,
                from: "k2t6j-2nvnp-4zjm3-25dtz-6xhaa-c7boj-5gayf-oj3xs-i43lp-teztq-6ae"
                    .parse()
                    .unwrap(),
                from_subaccount: None,
                created_at: Some(1699527697000000000),
            };
            apply_state_transition(
                state,
                &EventType::AcceptedEthWithdrawalRequest(withdrawal_request.clone()),
            );

            let max_fee = self.tx_fee.max_transaction_fee();
            let transaction = Eip1559TransactionRequest {
                chain_id: EthereumNetwork::Sepolia.chain_id(),
                nonce: self.nonce,
                max_priority_fee_per_gas: self.tx_fee.max_priority_fee_per_gas,
                max_fee_per_gas: self.tx_fee.max_fee_per_gas,
                gas_limit: self.tx_fee.gas_limit,
                destination: withdrawal_request.destination,
                amount: withdrawal_request
                    .withdrawal_amount
                    .checked_sub(max_fee)
                    .unwrap(),
                data: vec![],
                access_list: Default::default(),
            };
            apply_state_transition(
                state,
                &EventType::CreatedTransaction {
                    withdrawal_id: self.ledger_burn_index,
                    transaction: transaction.clone(),
                },
            );

            let dummy_signature = Eip1559Signature {
                signature_y_parity: false,
                r: Default::default(),
                s: Default::default(),
            };
            let signed_tx =
                SignedEip1559TransactionRequest::from((transaction.clone(), dummy_signature));
            apply_state_transition(
                state,
                &EventType::SignedTransaction {
                    withdrawal_id: self.ledger_burn_index,
                    transaction: signed_tx.clone(),
                },
            );

            let tx_receipt = TransactionReceipt {
                block_hash: "0xce67a85c9fb8bc50213815c32814c159fd75160acf7cb8631e8e7b7cf7f1d472"
                    .parse()
                    .unwrap(),
                block_number: BlockNumber::new(4190269),
                effective_gas_price: self.effective_gas_price,
                gas_used: signed_tx.transaction().gas_limit,
                status: self.tx_status,
                transaction_hash: signed_tx.hash(),
            };
            apply_state_transition(
                state,
                &EventType::FinalizedTransaction {
                    withdrawal_id: self.ledger_burn_index,
                    transaction_receipt: tx_receipt.clone(),
                },
            );
            tx_receipt
        }
    }
}
